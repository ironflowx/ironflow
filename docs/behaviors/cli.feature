# The CLI in release 0.3: `ironflowx` as an agent in a shell runs it, against a served engine.
# Conventions, adapters and what is out of scope: README.md. The rules these scenarios sample:
# architecture.md, "CLI".

Feature: CLI: generated commands, one envelope, and describe

  Background:
    Given the engine is served on a fresh data directory
    And the CLI is pointed at it with IRONFLOWX_URL
    And no other IRONFLOWX_ variable is set except those the scenario names

  # ---- Calling operations ----

  @CLI-001 @release-0.3 @door:cli
  Scenario Outline: An emit prints its acknowledgment; a get prints the data as sent
    Given IRONFLOWX_KEY holds a "write" and "read" key
    When "ironflowx events emit --name order.placed --data <data>" runs with stdout piped
    Then it exits with code 0 and stdout is one line holding the new "event_id" and "duplicate" false
    When "ironflowx events get --event-id" runs with that ID and stdout piped
    Then stdout is one line whose "data" is <printed>
    And whose "emitted_by" names the key

    Examples:
      | data                                                       | printed                       |
      | '{"id":9007199254740993}'                                  | {"id":9007199254740993}       |
      | @order.json, a file holding '{\n  "b": 1,\n  "a": [1, 2]\n}' | {"b":1,"a":[1,2]}             |

  @CLI-002 @release-0.3 @door:cli
  Scenario: Piped output is one line of JSON per answer
    Given IRONFLOWX_KEY holds the bootstrap key
    When "ironflowx whoami" runs with stdout piped
    Then stdout is exactly one line, a JSON object naming the key's ID and scopes
    And stderr is empty

  @CLI-014 @release-0.3 @door:cli
  Scenario: A whole request can be given as JSON
    Given IRONFLOWX_KEY holds a "write" and "read" key
    When "ironflowx events emit-batch --json -" runs with stdin holding two items named "a.one" and "a.two"
    Then it exits with code 0
    And stdout holds two results, each with an "event_id", in input order

  @CLI-013 @release-0.3 @door:cli
  Scenario: A key made from the CLI is read-only unless asked otherwise
    Given IRONFLOWX_KEY holds the bootstrap key
    When "ironflowx keys create --label agent" runs
    Then stdout holds the new key once, with scopes "read" only
    And "ironflowx keys list" shows the key with label "agent" and without its secret

  # ---- Keys ----

  @CLI-006 @release-0.3 @door:cli
  Scenario Outline: The key comes from a flag or a variable, the flag first
    Given a "read" key R and the bootstrap key A
    When "ironflowx whoami" runs with <given>
    Then stdout names key <named>

    Examples:
      | given                                              | named |
      | IRONFLOWX_KEY holding R                            | R     |
      | --key-file naming a file that holds R              | R     |
      | IRONFLOWX_KEY holding A and --key-file holding R   | R     |

  @CLI-007 @release-0.3 @door:cli
  Scenario: The CLI never picks up the admin key on its own
    Given the working directory is the one holding the data directory, with "admin.key" in it
    And no key is given
    When "ironflowx whoami" runs
    Then it exits with code 3 and an envelope with code "unauthenticated"

  # ---- Errors ----

  @CLI-003 @release-0.3 @door:cli
  Scenario Outline: Every failure is one envelope on stderr and a fixed exit code
    Given <setup>
    When "<command>" runs
    Then stdout is empty
    And stderr is one JSON object with code "<code>", a message, retryable <retryable> and a hint or none
    And it exits with code <exit>

    Examples:
      | setup                                  | command                                       | code               | retryable | exit |
      | IRONFLOWX_KEY holds a "write" key      | ironflowx events emit --name ""               | invalid            | false     | 2    |
      | IRONFLOWX_KEY holds a "write" key      | ironflowx events emit --json '{"name":"a.b","idempotncy_key":"k"}' | invalid | false | 2    |
      | IRONFLOWX_KEY holds a "write" key      | ironflowx events emit --json '{"name":"a.b","data":{"id":1}}' | invalid | false | 2    |
      | no key is given                        | ironflowx whoami                              | unauthenticated    | false     | 3    |
      | IRONFLOWX_KEY holds a "read" key       | ironflowx events emit --name a.b              | permission_denied  | false     | 4    |
      | IRONFLOWX_KEY holds a "read" key       | ironflowx events get --event-id an-unknown-id       | not_found          | false     | 5    |

  @CLI-004 @release-0.3 @door:cli
  Scenario: No engine at the address is unavailable and retryable
    Given IRONFLOWX_URL names an address where nothing listens
    And IRONFLOWX_KEY holds the bootstrap key
    When "ironflowx whoami" runs
    Then it exits with code 8 and an envelope with code "unavailable" and retryable true

  @CLI-015 @release-0.3 @door:cli
  Scenario: A usage error is an envelope too
    When "ironflowx events emit --no-such-flag" runs
    Then it exits with code 2 and an envelope with code "invalid" whose message names "--no-such-flag"

  # ---- Describe ----

  @CLI-008 @release-0.3 @door:cli
  Scenario: describe lists every operation with its scope, and nothing else
    Given no key is given
    And IRONFLOWX_URL names an address where nothing listens
    When "ironflowx describe" runs
    Then it exits with code 0
    And its operations are exactly:
      | command            | scope   | changes_state |
      | events emit        | write   | true          |
      | events emit-batch  | write   | true          |
      | events get         | read    | false         |
      | events list        | read    | false         |
      | keys create        | admin   | true          |
      | keys list          | admin   | false         |
      | keys revoke        | admin   | true          |
      | environments list  | read    | false         |
      | whoami             | any     | false         |

  @CLI-009 @release-0.3 @door:cli
  Scenario: describe gives an operation's schemas and an example that works
    When "ironflowx describe events emit" runs
    Then stdout holds an input schema, an output schema and an example
    And the example is valid against the input schema
    And with IRONFLOWX_KEY holding a "write" key, "ironflowx events emit --json" given the example exits with code 0
    And every described example works against a fresh engine with the bootstrap key
    And the published inputs enforce the wire limits, scopes and IDs
    And the batch output schema requires exactly one result per item

  @CLI-010 @release-0.3 @door:cli
  Scenario: describe publishes the exit-code table
    When "ironflowx describe" runs
    Then its exit codes are exactly:
      | code | kind               | retryable |
      | 0    | success            | false     |
      | 1    | internal           | false     |
      | 2    | invalid            | false     |
      | 3    | unauthenticated    | false     |
      | 4    | permission_denied  | false     |
      | 5    | not_found          | false     |
      | 6    | unknown            | false     |
      | 7    | resource_exhausted | false     |
      | 8    | unavailable        | true      |
      | 9    | conflict           | false     |

  # ---- Seeds, version ----

  @CLI-012 @release-0.3 @door:cli
  Scenario: A generated seed is a key the engine accepts
    When "ironflowx seed generate" runs
    Then stdout holds a "key" and its "key_id"
    And an engine served on another fresh data directory with that key as IRONFLOWX_SEED authenticates it as admin

  @CLI-016 @release-0.3 @door:cli
  Scenario: version answers in JSON
    When "ironflowx version" runs
    Then stdout is a JSON object with "version", "commit" and "protocol" "ironflowx.v1"
