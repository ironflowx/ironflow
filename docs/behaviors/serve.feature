# The host, the door and the wire in release 0.3: `ironflowx serve` as a process, called over the
# network. Conventions, adapters and what is out of scope: README.md. The rules these scenarios
# sample: architecture.md, "Wire" and "Host".

Feature: Serve: the host, the door and the wire

  Background:
    Given a fresh data directory
    And no IRONFLOWX_ variable is set except those the scenario names

  # ---- First start and the bootstrap key ----

  @HOST-001 @release-0.3
  Scenario: The first start writes the bootstrap key to a file only its owner can read
    When the engine is served on the data directory
    Then the engine becomes ready
    And the data directory has mode "0700"
    And "admin.key" exists in the data directory with mode "0400"
    And the log names the path of "admin.key" and does not contain the key
    And with the key in "admin.key", whoami names scopes "read", "write" and "admin"

  @HOST-002 @release-0.3
  Scenario: A second start writes no key and the first one still works
    Given the engine was served once and stopped
    And the operator kept the key from "admin.key" and deleted the file
    When the engine is served again
    Then "admin.key" does not exist
    And the kept key still authenticates

  @HOST-003 @release-0.3
  Scenario Outline: A seed becomes the bootstrap key and no file is written
    Given a seed key made with "ironflowx seed generate"
    When the engine is served with the seed given <how>
    Then "admin.key" does not exist
    And the seed key authenticates with scopes "read", "write" and "admin"

    Examples:
      | how                                       |
      | in a file named by --seed-file            |
      | in a file named by IRONFLOWX_SEED_FILE    |
      | as the value of IRONFLOWX_SEED            |

  @HOST-004 @release-0.3
  Scenario: Two seeds at once are refused before anything is written
    Given a seed key A in a file named by --seed-file
    And a seed key B as the value of IRONFLOWX_SEED
    When the engine is served
    Then serve exits with code 2 and an envelope with code "invalid"
    And the data directory does not exist

  @HOST-005 @release-0.3
  Scenario: A malformed seed is refused and never echoed
    Given "not-a-key-but-secret" as the value of IRONFLOWX_SEED
    When the engine is served
    Then serve exits with code 2 and an envelope that names the key format
    And neither stderr nor stdout contains "not-a-key-but-secret"

  @HOST-019 @release-0.3
  Scenario: A revoked seed starts the engine with a warning
    Given the engine was served with seed key A, which created an admin key K and was revoked by K, and stopped
    When the engine is served with seed key A
    Then the engine becomes ready
    And the log warns that the seed is revoked
    And key A is refused as an invalid key

  @HOST-017 @release-0.3
  Scenario Outline: A lost admin key is recovered while the engine runs
    Given the engine is served and its bootstrap key revoked itself
    And "admin.key" was deleted
    When "ironflowx recover-key" runs on the data directory <with>
    Then <where> holds a key that authenticates with scopes "read", "write" and "admin"
    And listing keys shows it with origin "recovered"

    Examples:
      | with                 | where                                 |
      | and no other flag    | "admin.key", with mode "0400",        |
      | and --stdout         | the "key" field of its JSON output    |

  @HOST-018 @release-0.3
  Scenario: Recovery never overwrites a key file, and mints nothing when it cannot write one
    Given the engine is served and "admin.key" is still there
    When "ironflowx recover-key" runs on the data directory
    Then it exits with code 2 and an envelope whose hint names --stdout
    And "admin.key" is unchanged
    And with the key in "admin.key", listing keys shows no key with origin "recovered"

  @HOST-030 @release-0.3
  Scenario: A key file left from another install stops the start
    Given a data directory holding only an "admin.key" file
    When the engine is served on it
    Then serve exits with code 2 and an envelope that names "admin.key"
    And the data directory holds only that "admin.key", unchanged

  # ---- Health and readiness ----

  @HOST-006 @release-0.3
  Scenario: Health and readiness answer without a key
    When the engine is served and becomes ready
    Then GET "/health" answers 200 with no key
    And GET "/ready" answers 200 with no key
    And "ironflowx describe" lists neither

  # ---- Starting, stopping and refusing ----

  @HOST-009 @release-0.3
  Scenario: An address in use is refused before the data directory is touched
    Given another process listens on the address the engine is given
    When the engine is served
    Then serve exits with code 8 and an envelope with code "unavailable" and retryable true
    And the data directory does not exist

  @HOST-010 @release-0.3
  Scenario: A data directory another engine serves is refused
    Given the engine is served on the data directory
    When a second engine is served on the same data directory and another address
    Then the second exits with code 8 and an envelope with code "unavailable"
    And the first still answers calls

  @HOST-011 @release-0.3
  Scenario: A data directory another version wrote is refused
    Given a data directory whose control plane has another format version
    When the engine is served on it
    Then serve exits with code 1 and an envelope with code "internal" that names the format version
    And "control.db" and "ledgers/" are unchanged

  @HOST-012 @release-0.3
  Scenario: A stop keeps every acknowledged event
    Given the engine is served and a client emitted 100 events and saw each acknowledged
    When serve receives SIGTERM
    Then serve exits with code 0 within 11 seconds
    And after the engine is served again, all 100 events read back

  @HOST-013 @release-0.3
  Scenario Outline: A listen address off this machine warns that keys travel in clear
    When the engine is served on <address>
    Then the log <does> warn that keys travel in clear without a TLS proxy

    Examples:
      | address          | does     |
      | "127.0.0.1:0"    | does not |
      | "0.0.0.0:0"      | does     |

  @HOST-016 @release-0.3
  Scenario: The bound address is logged when the system picks it
    When the engine is served on "127.0.0.1:0"
    Then the log has one "listening" line whose URL answers GET "/health" with 200

  # ---- Settings ----

  @HOST-014 @release-0.3
  Scenario: An unknown IRONFLOWX_ variable is warned, not ignored in silence
    Given IRONFLOWX_LISEN is set to "127.0.0.1:0"
    When the engine is served
    Then the log warns once that IRONFLOWX_LISEN is not a setting

  @HOST-015 @release-0.3
  Scenario: A flag beats its variable
    Given IRONFLOWX_DATA_DIR names directory A
    When the engine is served with --data-dir naming directory B
    Then directory B holds the engine and directory A does not exist

  # ---- The wire ----

  @HOST-021 @release-0.3
  Scenario Outline: The key is read only from the Authorization header
    Given the engine is served and an admin key is known
    When whoami is called over Connect with <credential>
    Then the call <answer>

    Examples:
      | credential                                         | answer                                                             |
      | "Authorization: Bearer " and the admin key         | succeeds and names the admin key's ID                              |
      | no header                                          | is refused as unauthenticated with "authentication required"       |
      | "Authorization: Bearer" and nothing else           | is refused as unauthenticated with "authentication required"       |
      | "Authorization: bearer " and the admin key         | succeeds and names the admin key's ID                              |
      | "Authorization: " and the admin key, no scheme     | is refused as unauthenticated with "invalid API key"               |
      | "Authorization: Basic " and the admin key          | is refused as unauthenticated with "invalid API key"               |
      | the admin key in a query parameter "token"         | is refused as unauthenticated with "authentication required"       |
      | the admin key in a header "X-Test-Credential"      | is refused as unauthenticated with "authentication required"       |

  @HOST-022 @release-0.3
  Scenario: A missing scope crosses the wire with its hint
    Given the engine is served and a client holds a "read" key
    When the client emits an event over Connect
    Then the call is refused as permission denied with "missing scope: write"
    And the hint says a key with scope "write" is needed
    And the error is not retryable

  @HOST-024 @release-0.3
  Scenario Outline: Event data comes back exactly as sent, on every protocol
    Given the engine is served and a client holds a "write" and "read" key
    When the client emits an event with data <data> over <codec>
    And reads it back over <codec>
    Then the data is byte for byte <data>
    And over JSON the event's time is RFC 3339 in UTC, with no fraction finer than a millisecond

    Examples:
      | data                                     | codec              |
      | { "b": 1,  "a": 2 }                      | Connect JSON       |
      | {"id": 9007199254740993}                 | Connect JSON       |
      | {"id": 9007199254740993}                 | Connect binary     |
      | ["é", "é", 1.50]                    | gRPC               |
      | built: an object spread over two lines   | gRPC-Web           |

  @HOST-023 @release-0.3
  Scenario: A request body over 16 MiB is refused and stores nothing
    Given the engine is served, a client holds a "write" key and an observer holds a "read" key
    When the client sends an emit batch whose request body is 17 MiB
    Then the call is refused as resource exhausted
    And the observer lists no events

  @HOST-025 @release-0.3
  Scenario Outline: Text that is not UTF-8 on the binary wire is refused
    Given the engine is served, a client holds a "write" key and an observer holds a "read" key
    When the client sends an emit over Connect binary whose <field> holds the bytes FF FE
    Then the call is refused as invalid
    And the observer lists no events

    Examples:
      | field            |
      | name             |
      | idempotency key  |
      | data             |
      | metadata         |

  @HOST-026 @release-0.3
  Scenario: A key expiry finer than a millisecond is refused
    Given the engine is served and a client holds the bootstrap key
    When the client creates a key over Connect whose expiry has 1 nanosecond past a whole millisecond
    Then the call is refused as invalid
    And listing keys shows only the bootstrap key

  @HOST-027 @release-0.3
  Scenario Outline: Each served protocol reaches the same operation
    Given the engine is served and an admin key is known
    When whoami is called over <protocol> with the admin key
    Then the answer names the admin key's ID

    Examples:
      | protocol       |
      | Connect JSON   |
      | Connect binary |
      | gRPC           |
      | gRPC-Web       |

  @HOST-029 @release-0.3
  Scenario: A failed batch item keeps its kind and message over the wire
    Given the engine is served and a client holds a "write" key
    When the client emits a batch over Connect of an event named "a.one" and an event with no name
    Then the first result is an event
    And the second result is an error of kind invalid with message "event name is required"
