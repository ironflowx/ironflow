#!/usr/bin/env bash
# Usage: ci/no-release-fault-points.sh [release.yml] [Cargo.toml]
# Archive builds use a direct, package-only canonical Cargo built-in (`build`, `rustc`, `run`) in
# release.yml; aliasable shorthands (`b`, `r`, `zigbuild`) and custom aliases are rejected. Steps
# that do not mention cargo, cross, __fault-points, --all-features, or CARGO_ALIAS_ may use
# arbitrary shell syntax. Any step that mentions them must invoke cargo/cross directly (no
# wrappers, heredocs, arithmetic shifts, or process substitutions) and select only ironflowx-bin
# without dev targets, private features, or alias overrides. Standalone echo/printf logging of
# cargo text is exempt only outside pipes, redirects, and substitutions in steps with no unquoted
# grouping or compound-keyword tokens ((, ), {, }, if/then/.../esac), so logging in a step with an
# unrelated if/brace block or an unquoted keyword argument is refused (release.yml has none).
# Commands hidden in external scripts or constructed from shell variables are outside this check.
set -euo pipefail

workflow=${1:-.github/workflows/release.yml}
ruby -ryaml - "$workflow" <<'RUBY'
  CARGO_GLOBAL_VALUE_FLAGS = %w[-C --color --config -Z --explain].freeze
  CARGO_BUILD_SUBCOMMANDS = %w[build rustc run].freeze
  CARGO_READONLY_SUBCOMMANDS = %w[
    metadata version help fetch tree pkgid locate-project verify-project
  ].freeze
  COMPOUND_WORDS = %w[{ } if then elif else fi for while until do done case esac].freeze
  DESCRIPTIVE_KEYS = %w[name description].freeze
  CARGO_WORD = /(?<![A-Za-z0-9_])(?:cargo|cross)(?![A-Za-z0-9_])/.freeze
  INTERESTING =
    /(?<![A-Za-z0-9_])(?:cargo|cross)(?![A-Za-z0-9_])|__fault-points|--all-features|(?i:CARGO_ALIAS_)/.freeze
  ALIAS_ENV = /(?i:CARGO_ALIAS_)/.freeze
  ALIAS_CONFIG_KEY = /\A\s*["']?alias["']?\s*(?:\.|=)/.freeze

  # Extract the inner body of $(...) starting at '('. Returns [inner, next_index].
  def extract_parens(script, open_idx)
    i = open_idx + 1
    depth = 1
    in_single = false
    in_double = false
    at_word_start = true
    while i < script.length && depth > 0
      ch = script[i]
      if in_single
        in_single = false if ch == "'"
      elsif in_double
        if ch == "\\" && i + 1 < script.length
          i += 1
        elsif ch == '"'
          in_double = false
        end
      elsif ch == "#" && at_word_start
        nl = script.index("\n", i)
        break unless nl
        i = nl
        at_word_start = true
      elsif ch == "\\" && i + 1 < script.length
        if script[i + 1] == "\n"
          i += 2
          next
        elsif script[i + 1, 2] == "\r\n"
          i += 3
          next
        end
        i += 1
        at_word_start = false
      elsif ch == "'"
        in_single = true
        at_word_start = false
      elsif ch == '"'
        in_double = true
        at_word_start = false
      elsif ch == "("
        depth += 1
        at_word_start = true
      elsif ch == ")"
        depth -= 1
        return [script[(open_idx + 1)...i], i + 1] if depth == 0
        at_word_start = true
      else
        at_word_start = " \t\r\n;&|".include?(ch)
      end
      i += 1
    end
    raise ArgumentError, "unterminated $("
  end

  # Extract the inner body of `...` starting at the opening backtick. Returns [inner, next_index].
  def extract_backticks(script, open_idx)
    i = open_idx + 1
    inner = +""
    while i < script.length
      ch = script[i]
      if ch == "\\" && i + 1 < script.length
        nxt = script[i + 1]
        if nxt == "\n"
          i += 2
          next
        elsif nxt == "\r" && script[i + 2] == "\n"
          i += 3
          next
        elsif %w[` $ \\].include?(nxt)
          inner << nxt
          i += 2
          next
        end
      elsif ch == "`"
        return [inner, i + 1]
      end
      inner << ch
      i += 1
    end
    raise ArgumentError, "unterminated backtick"
  end

  # Strip simple redirections and their targets, returning [word, quoted] pairs.
  def strip_redirs(items)
    words = []
    i = 0
    while i < items.length
      kind, text, quoted = items[i]
      if kind == :redir
        i += 1
        i += 1 if i < items.length && items[i][0] == :word
      else
        words << [text, quoted]
        i += 1
      end
    end
    words
  end

  # Tokenize a step that mentions cargo/cross or private feature flags. Returns
  # [commands, unsupported_syntax, has_grouping]. Heredocs (<<), process substitutions (<( or >()),
  # and arithmetic (( fail closed. Any unquoted grouping/compound token latches has_grouping.
  def commands(script, nested: false)
    out = []
    current = []
    word = +""
    in_word = false
    word_quoted = false
    piped_prev = false
    has_grouping = false
    unsupported = false

    flush_word = lambda do
      if in_word
        current << [:word, word, word_quoted]
        word = +""
        in_word = false
        word_quoted = false
      end
    end
    flush_cmd = lambda do |piped_next|
      flush_word.call
      redirected = current.any? { |kind, _, _| kind == :redir }
      pairs = strip_redirs(current)
      while !pairs.empty? && !pairs.first[1] && COMPOUND_WORDS.include?(pairs.first[0])
        has_grouping = true
        pairs.shift
      end
      has_grouping ||= pairs.any? { |text, quoted| !quoted && COMPOUND_WORDS.include?(text) }
      words = pairs.map(&:first)
      unless words.empty?
        out << {
          words: words,
          piped: piped_prev || piped_next,
          nested: nested,
          redirected: redirected
        }
      end
      current = []
      piped_prev = piped_next
    end

    i = 0
    n = script.length
    while i < n
      ch = script[i]
      if ch == "\\" && i + 1 < n
        if script[i + 1] == "\n"
          i += 2
        elsif script[i + 1, 2] == "\r\n"
          i += 3
        else
          in_word = true
          word_quoted = true
          word << script[i + 1]
          i += 2
        end
      elsif ch == "'"
        in_word = true
        word_quoted = true
        i += 1
        close = script.index("'", i)
        return [out, true, has_grouping] unless close
        word << script[i...close]
        i = close + 1
      elsif ch == '"'
        in_word = true
        word_quoted = true
        i += 1
        closed = false
        while i < n
          c = script[i]
          if c == "\\" && i + 1 < n
            nxt = script[i + 1]
            if nxt == "\n"
              i += 2
            elsif nxt == "\r" && script[i + 2] == "\n"
              i += 3
            elsif %w[" \\ $ `].include?(nxt)
              word << nxt
              i += 2
            else
              word << c << nxt
              i += 2
            end
          elsif c == '"'
            i += 1
            closed = true
            break
          elsif c == "$" && script[i + 1] == "("
            return [out, true, has_grouping] if script[i + 2] == "("
            inner, i = extract_parens(script, i + 1)
            sub_cmds, sub_unsup, = commands(inner, nested: true)
            out.concat(sub_cmds)
            unsupported ||= sub_unsup
          elsif c == "`"
            inner, i = extract_backticks(script, i)
            sub_cmds, sub_unsup, = commands(inner, nested: true)
            out.concat(sub_cmds)
            unsupported ||= sub_unsup
          else
            word << c
            i += 1
          end
        end
        return [out, true, has_grouping] unless closed
      elsif ch == "#" && !in_word
        nl = script.index("\n", i)
        i = nl || n
      elsif ch == "$" && script[i + 1] == "("
        return [out, true, has_grouping] if script[i + 2] == "("
        in_word = true
        inner, i = extract_parens(script, i + 1)
        sub_cmds, sub_unsup, = commands(inner, nested: true)
        out.concat(sub_cmds)
        unsupported ||= sub_unsup
      elsif ch == "`"
        in_word = true
        inner, i = extract_backticks(script, i)
        sub_cmds, sub_unsup, = commands(inner, nested: true)
        out.concat(sub_cmds)
        unsupported ||= sub_unsup
      elsif script[i, 2] == "<<" || script[i, 2] == ">(" || script[i, 2] == "<(" || script[i, 2] == "(("
        return [out, true, has_grouping]
      elsif ch == "<" || ch == ">" || (ch == "&" && script[i + 1] == ">")
        if ch != "&" && in_word && !word_quoted && word.match?(/\A[0-9]+\z/)
          word = +""
          in_word = false
        else
          flush_word.call
        end
        op =
          if script[i, 3] == "&>>"
            script[i, 3]
          elsif %w[<& <> >> >& >| &>].include?(script[i, 2])
            script[i, 2]
          else
            ch
          end
        current << [:redir, op, false]
        i += op.length
      elsif ch == " " || ch == "\t" || ch == "\r"
        flush_word.call
        i += 1
      elsif script[i, 2] == "||" || script[i, 2] == "&&"
        flush_cmd.call(false)
        i += 2
      elsif ch == "|"
        flush_cmd.call(true)
        i += 1
      elsif ch == "(" || ch == ")"
        flush_cmd.call(false)
        has_grouping = true
        i += 1
      elsif "\n;&".include?(ch)
        flush_cmd.call(false)
        i += 1
      else
        in_word = true
        word << ch
        i += 1
      end
    end
    flush_cmd.call(false)
    [out, unsupported, has_grouping]
  rescue ArgumentError
    [out, true, has_grouping]
  end

  def split_env(tokens)
    env = []
    i = 0
    while i < tokens.length && tokens[i].match?(/\A[A-Za-z_][A-Za-z0-9_]*=/)
      env << tokens[i]
      i += 1
    end
    [env, tokens.drop(i)]
  end

  def find_subcommand(args)
    i = 0
    while i < args.length
      arg = args[i]
      if arg.start_with?("+")
        i += 1
      elsif CARGO_GLOBAL_VALUE_FLAGS.include?(arg)
        i += 2
      elsif arg.start_with?("-")
        i += 1
      else
        return [arg, i]
      end
    end
    nil
  end

  def config_values(args)
    values = []
    i = 0
    while i < args.length
      case args[i]
      when "--config"
        values << args[i + 1] if i + 1 < args.length
        i += 2
      when /\A--config=(.*)\z/m
        values << Regexp.last_match(1)
        i += 1
      else
        i += 1
      end
    end
    values
  end

  def executable_strings(node, in_env: false)
    case node
    when Hash
      node.flat_map do |key, value|
        next [] if !in_env && (key == "run" || DESCRIPTIVE_KEYS.include?(key))
        out = in_env ? [key.to_s] : []
        out + executable_strings(value, in_env: in_env || key == "env")
      end
    when Array then node.flat_map { |value| executable_strings(value, in_env: in_env) }
    when String then [node]
    else []
    end
  end

  def check_tokens(tokens, label, bad)
    if tokens.any? { |text| text.include?("__fault-points") }
      bad << "#{label}: release workflow names __fault-points"
    end
    if tokens.any? { |text| text.include?("--all-features") }
      bad << "#{label}: release workflow enables --all-features"
    end
    if tokens.any? { |text| text.match?(ALIAS_ENV) }
      bad << "#{label}: release Cargo build must select only ironflowx-bin without dev targets"
    end
  end

  path = ARGV.fetch(0)
  workflow = YAML.load_file(path)
  bad = []
  top_strings = executable_strings(workflow.reject { |key, _| key == "jobs" })
  check_tokens(top_strings, "workflow", bad)

  workflow.fetch("jobs").each do |name, job|
    check_tokens(executable_strings(job), name, bad)
    job.fetch("steps", []).each do |step|
      script = step["run"]
      next unless script.is_a?(String)
      probe = script.gsub(/\\\r?\n/, "").gsub(/[\\'"]/, "")
      next unless probe.match?(INTERESTING)

      cmds, unsupported, has_grouping = commands(script)
      if unsupported
        bad << "#{name}: release Cargo build must select only ironflowx-bin without dev targets"
      end
      cmds.each do |entry|
        env, cmd = split_env(entry[:words])
        check_tokens(env, name, bad)
        next if cmd.empty?
        exe = File.basename(cmd.first)
        args = cmd.drop(1)
        if %w[cargo cross].include?(exe)
          cargo_args = args.take_while { |arg| arg != "--" }
          check_tokens(cargo_args, name, bad)
          if config_values(cargo_args).any? { |val| val.match?(ALIAS_CONFIG_KEY) }
            bad << "#{name}: release Cargo build must select only ironflowx-bin without dev targets"
            next
          end
          sub, sub_idx = find_subcommand(cargo_args)
          next unless sub
          next if CARGO_READONLY_SUBCOMMANDS.include?(sub)
          unless CARGO_BUILD_SUBCOMMANDS.include?(sub)
            bad << "#{name}: release Cargo build must select only ironflowx-bin without dev targets"
            next
          end
          sub_args = cargo_args.drop(sub_idx + 1)
          packages = sub_args.each_index.map do |i|
            case sub_args[i]
            when "-p", "--package" then sub_args[i + 1]
            when /\A--package=(.+)\z/, /\A-p(.+)\z/ then Regexp.last_match(1)
            end
          end.compact
          broad = sub_args.any? do |arg|
            arg.match?(/\A--(?:workspace|all|all-targets|tests?|examples?|benches|bench)(?:=|\z)/)
          end
          if packages != ["ironflowx-bin"] || broad
            bad << "#{name}: release Cargo build must select only ironflowx-bin without dev targets"
          end
        else
          check_tokens(cmd, name, bad)
          standalone_output =
            %w[echo printf].include?(exe) &&
            !entry[:piped] && !entry[:nested] && !entry[:redirected] && !has_grouping
          if !standalone_output && cmd.any? { |tok| tok.match?(CARGO_WORD) }
            bad << "#{name}: release Cargo build must select only ironflowx-bin without dev targets"
          end
        end
      end
    end
  end
  puts bad.uniq.map { |message| "#{path}: #{message}" }
  exit bad.empty?
RUBY

manifest=${2:-Cargo.toml}
# Even a regular opt-in feature of the host must not be an alias for a crash feature.
ordinary=$(cargo metadata --locked --manifest-path "$manifest" --no-deps --format-version 1 |
  jq -er 'first(.packages[] | select(.name == "ironflowx-bin"))
    | .features | keys | map(select(. != "__fault-points")) | join(",")')
features=$(cargo tree --locked --manifest-path "$manifest" -p ironflowx-bin \
  --target all --edges normal,build --features "$ordinary" \
  --prefix none --format '{p} [{f}]')
if grep -qE '[[,]__fault-points[],]' <<<"$features"; then
  echo "ironflowx-bin: ordinary release graph enables __fault-points"
  exit 1
fi
