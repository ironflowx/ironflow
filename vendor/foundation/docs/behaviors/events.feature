# Events: emit, batch emit, idempotency, reading events back by ID and by page,
# and read your writes, through Operations, each call with a key.
# Conventions, adapters and what is out of scope: README.md. The rules these
# scenarios sample: architecture.md, "Event rules".

Feature: Emitting events and reading them back

  Background:
    Given a fresh environment
    And a client

  # ---- Emit ----

  @EVT-001 @release-0.1
  Scenario: Emitting an event returns its ID
    When the client emits event "order.placed" with data {"order_id": "o-1"}
    Then the emit succeeds with a UUID event ID

  @EVT-002 @release-0.1
  Scenario: An emitted event reads back with what was sent
    When the client emits event "order.placed" with data {"order_id": "o-1"}, metadata {"trace": "t-1"} and idempotency key "k-1"
    And the client reads that event by ID
    Then it has name "order.placed", data {"order_id": "o-1"}, metadata {"trace": "t-1"}, version 1 and idempotency key "k-1"

  @EVT-003 @release-0.1
  Scenario Outline: A non-object payload round-trips
    When the client emits event "value.set" with data value <value>
    And the client reads that event by ID
    Then its data is <value>

    Examples:
      | value       |
      | false       |
      | ["payload"] |
      | null        |
      | "text"      |
      | 42          |

  @EVT-004 @release-0.1
  Scenario: An event emitted without data stores null
    When the client emits event "ping" with no data
    And the client reads that event by ID
    Then its data is null

  @EVT-008 @release-0.1
  Scenario Outline: An out-of-range version is refused
    When the client emits event "order.placed" with version <version>
    Then the emit is refused as invalid
    And no event is stored

    Examples:
      | version    |
      | -1         |
      | 0          |
      | 2147483648 |

  @EVT-009 @release-0.1
  Scenario Outline: A version in range reads back as sent
    When the client emits event "order.placed" with version <version>
    And the client reads that event by ID
    Then it has version <version>

    Examples:
      | version    |
      | 1          |
      | 3          |
      | 2147483647 |

  # ---- Emit gates ----

  @EVT-010 @release-0.1
  Scenario: An event needs a name
    When the client emits an event with no name
    Then the emit is refused as invalid with "event name is required"
    And no event is stored

  @EVT-011 @release-0.1
  Scenario Outline: An invalid event name is refused and nothing is stored
    When the client emits event <name>
    Then the emit is refused as invalid
    And no event is stored

    Examples:
      | name                        | why                      |
      | a name of 257 ASCII letters | over the length limit    |
      | "order placed"              | whitespace               |
      | "order\u0000placed"         | control character        |
      | "a..b"                      | empty segment            |
      | ".a"                        | empty segment            |
      | "a."                        | empty segment            |
      | "order\u00a0placed"         | Unicode whitespace       |
      | a name of 129 "é"           | 258 bytes, over the limit |

  @EVT-012 @release-0.1
  Scenario Outline: Unicode names, names at the limit and other-case owned roots are accepted
    When the client emits event <name>
    Then the emit succeeds

    Examples:
      | name                  |
      | "订单.placed"          |
      | a name of 256 ASCII letters |
      | a name of 128 "é"     |
      | "Ironflowx.run.done"  |
      | "ironflowxy.done"     |
      | "$System.tick"        |
      | "$custom.tick"        |

  @EVT-013 @release-0.1
  Scenario Outline: Names the engine owns are refused
    When the client emits event <name>
    Then the emit is refused as invalid
    And no event is stored

    Examples:
      | name                  |
      | "ironflowx"           |
      | "ironflowx.run.done"  |
      | "$system"             |
      | "$system.tick"        |

  @EVT-014 @release-0.1
  Scenario Outline: A top-level redaction marker in the payload is refused, whatever its value
    When the client emits event "order.placed" with data <data>
    Then the emit is refused as invalid
    And no event is stored

    Examples:
      | data                            |
      | {"$redacted": true}             |
      | {"$redacted": false}            |
      | {"a": 1, "$redacted": null}     |

  @EVT-015 @release-0.1
  Scenario: A nested redaction marker is accepted
    When the client emits event "order.placed" with data {"inner": {"$redacted": true}}
    Then the emit succeeds

  @EVT-016 @release-0.1
  Scenario Outline: Malformed event data is refused
    When the client emits event "order.placed" whose <part> is <text>
    Then the emit is refused as invalid
    And no event is stored

    Examples:
      | part            | text                               | why                           |
      | data            | {"a": 1, "a": 2}                   | duplicate key                 |
      | data            | {"x": {"a": 1, "a": 2}}            | duplicate key, nested         |
      | data            | {"a": 1, "\u0061": 2}              | keys equal after unescaping   |
      | data            | "x\u0000y"                         | NUL in a top-level string     |
      | data            | {"k": "x\u0000y"}                  | NUL in a nested string        |
      | data            | {"k\u0000": 1}                     | NUL in a key                  |
      | data            | {"k": "\ud800"}                    | unpaired high surrogate       |
      | data            | {"k": "\udc00"}                    | lone low surrogate            |
      | data            | {"k": 1                            | not JSON                      |
      | data            | built: 128 nested arrays           | deeper than 127               |
      | data            | 1e400                              | number out of range           |
      | data            | ["ok", "\ud800"]                   | unpaired surrogate in an array |
      | data            | built: a string holding the byte 0xFF | invalid UTF-8              |
      | metadata        | {"t": 1, "t": 2}                   | duplicate key                 |
      | metadata        | ["t"]                              | not an object                 |
      | metadata        | {"t": "x\u0000"}                   | NUL                           |
      | metadata        | {"t": "\udc00"}                    | lone low surrogate            |
      | metadata        | built: an object whose "t" is a string holding the byte 0xFF | invalid UTF-8 |
      | idempotency key | "k\u0000"                          | NUL                           |

  @EVT-017 @release-0.1
  Scenario: Data over 524,288 bytes as sent is refused, and data at the limit is accepted
    When the client emits event "blob.saved" with data {"blob": "<ASCII a repeated>"} sent as exactly 524,289 bytes
    Then the emit is refused as invalid, and the message contains "event data exceeds maximum size"
    And no event is stored
    When the client emits event "blob.saved" with data {"blob": "<ASCII a repeated>"} padded with spaces to exactly 524,289 bytes
    Then the emit is refused as invalid
    When the client emits event "blob.saved" with data {"blob": "<ASCII a repeated>"} sent as exactly 524,288 bytes
    Then the emit succeeds

  @EVT-038 @release-0.1
  Scenario: Metadata over 65,536 bytes as sent is refused, and metadata at the limit is accepted
    When the client emits event "order.placed" with metadata {"t": "<ASCII a repeated>"} sent as exactly 65,537 bytes
    Then the emit is refused as invalid
    And no event is stored
    When the client emits event "order.placed" with metadata {"t": "<ASCII a repeated>"} sent as exactly 65,536 bytes
    Then the emit succeeds

  @EVT-039 @release-0.1
  Scenario Outline: Data at the edges is accepted and reads back byte for byte
    When the client emits event "edge.case" with data <data>
    And the client reads that event by ID
    Then its data is exactly the bytes sent

    Examples:
      | data                       |
      | built: 127 nested arrays   |
      | {"k": "\ud83d\ude00"}      |
      | {"n": 1.10, "big": 123456789012345678901234567890} |
      | {  "spaced" :  true  }     |

  # ---- Batch ----

  @EVT-020 @release-0.1
  Scenario: A batch reports a bad item and still emits the others
    When the client emits a batch of "a.ok", an event with no name, and "b.ok"
    Then the batch call succeeds
    And the second item is refused as invalid and has no event ID
    And events "a.ok" and "b.ok" are stored

  @EVT-021 @release-0.1
  Scenario: A batch holds at most 100 events
    When the client emits a batch of 101 events
    Then the batch is refused as invalid with "batch size 101 exceeds maximum of 100"
    And no event is stored

  @EVT-022 @release-0.1
  Scenario: A key repeated inside one batch returns the earlier item's event
    When the client emits a batch of "a.ok" with idempotency key "k-1" and "b.ok" with idempotency key "k-1"
    Then both items return the same event ID
    And the second item is a duplicate
    And only "a.ok" is stored

  @EVT-023 @release-0.1
  Scenario: An empty batch answers with no results
    When the client emits a batch of no events
    Then the batch call succeeds with no results
    And no event is stored

  @EVT-024 @release-0.1
  Scenario: A batch's events commit in input order
    When the client emits a batch of "b.1", "b.2" and "b.3"
    And the client lists events
    Then they come back as "b.3", "b.2", "b.1"

  @EVT-025 @release-0.1
  Scenario: A batch whose accepted data totals over 4 MiB is refused whole
    When the client emits a batch of 9 events, each with data of 524,288 bytes
    Then the batch is refused as invalid
    And no event is stored

  # ---- Idempotency ----

  @EVT-030 @release-0.1
  Scenario: Repeating an idempotency key returns the first event and stores nothing new
    When the client emits event "order.placed" with idempotency key "order-1"
    And the client emits event "order.placed" with idempotency key "order-1" again
    Then both emits return the same event ID
    And one event is stored

  @EVT-031 @release-0.1
  Scenario: Without a key every emit is a new event
    When the client emits event "order.placed" twice without an idempotency key
    Then the emits return different event IDs
    And two events are stored

  @EVT-032 @release-0.1
  Scenario: A key is not scoped by event name
    When the client emits event "order.placed" with idempotency key "k-1"
    And the client emits event "order.cancelled" with idempotency key "k-1"
    Then the second emit returns the first event's ID
    And only "order.placed" is stored

  @EVT-033 @release-0.1 @local
  Scenario: A key holds for as long as its event exists
    Given the client emitted event "order.placed" with idempotency key "order-1" 30 days ago
    When the client emits event "order.placed" with idempotency key "order-1"
    Then the emit returns the original event ID

  @EVT-035 @release-0.1
  Scenario Outline: Idempotency keys that are not malformed are accepted
    When the client emits event "order.placed" with idempotency key <key>
    Then the emit succeeds

    Examples:
      | key               |
      | "order-1"         |
      | a key of 256 ASCII letters |
      | "literal \\\\u0000" |

  @EVT-036 @release-0.1
  Scenario Outline: Every check runs before the idempotency lookup
    Given the client emitted event "order.placed" with idempotency key "order-1"
    When the client emits <request> with idempotency key "order-1"
    Then the emit is refused as invalid
    And one event is stored

    Examples:
      | request                                        |
      | event "order placed"                           |
      | event "order.placed" with data {"a": 1, "a": 2} |
      | event "order.placed" with version -1           |

  @EVT-037 @release-0.1
  Scenario Outline: A malformed idempotency key is refused
    When the client emits event "order.placed" with idempotency key <key>
    Then the emit is refused as invalid
    And no event is stored

    Examples:
      | key                        |
      | ""                         |
      | a key of 257 ASCII letters |

  # ---- Reading events ----

  @EVT-040 @release-0.1
  Scenario Outline: An unknown or unparseable event ID is not found
    When the client reads event <id>
    Then the read is refused as not found with "event not found"

    Examples:
      | id                                     |
      | "019a8f3c-7e21-7b4a-9c3d-5e6f7a8b9c0d" |
      | "not-a-uuid"                           |

  @EVT-041 @release-0.1
  Scenario: Events list newest first, in commit order
    Given the client emitted events "e.1", "e.2" and "e.3" in that order
    When the client lists events
    Then they come back as "e.3", "e.2", "e.1"

  @EVT-042 @release-0.1
  Scenario: Events page by cursor
    Given the client emitted 5 events
    When the client lists events with a limit of 2
    Then the page holds 2 events and has a next page
    When the client follows the next cursor with a limit of 3
    Then the page holds 3 events and has no next page

  @EVT-043 @release-0.1
  Scenario: A cursor is stable while new events arrive
    Given the client emitted 4 events and listed the first page of 2
    When the client emits 3 more events and follows the next cursor
    Then the page holds the next 2 of the original events

  @EVT-045 @release-0.1
  Scenario: A cursor that is not a token is refused
    When the client lists events with a cursor that is not a token
    Then the list is refused as invalid

  @EVT-046 @release-0.1
  Scenario Outline: The page limit defaults to 20 and clamps at 200
    Given the client emitted 250 events
    When the client lists events with <limit>
    Then the page holds <count> events

    Examples:
      | limit       | count |
      | no limit    | 20    |
      | limit 500   | 200   |

  @EVT-053 @release-0.1
  Scenario: An empty environment lists no events and no next page
    When the client lists events
    Then the page holds 0 events and has no next page

  @EVT-054 @release-0.1
  Scenario: The last page has no next cursor, even when it is full
    Given the client emitted 4 events
    When the client lists events with a limit of 2
    And the client follows the next cursor with a limit of 2
    Then the page holds 2 events and has no next page

  @EVT-055 @release-0.1 @local
  Scenario: A cursor from another incarnation of the environment is refused
    Given a cursor issued by an environment with the same ID and another incarnation token
    When the client lists events with that cursor
    Then the list is refused as invalid

  # ---- Read your writes ----

  @EVT-060 @release-0.1
  Scenario: An emitted event is readable as soon as the emit returns
    When the client emits event "order.placed"
    Then reading that event by ID right away finds it
    And listing events right away includes it

  @EVT-061 @release-0.1
  Scenario: A refused emit leaves nothing behind
    When the client emits event "a..b"
    Then listing events returns none

  @EVT-062 @release-0.1
  Scenario: Another client reads an event as soon as its emit returns
    Given another client
    When the client emits event "order.placed"
    Then the other client reading that event by ID right away finds it
    And the other client listing events right away includes it
