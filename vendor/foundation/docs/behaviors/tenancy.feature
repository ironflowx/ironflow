# Tenancy in release 0.2: bootstrap, keys, scopes and who emitted each event, through
# the engine. Conventions, adapters and what is out of scope: README.md. The rules these
# scenarios sample: architecture.md, "Keys", "Errors" and "Engine open".

Feature: Tenancy: bootstrap, keys and scopes

  Background:
    Given a fresh engine directory

  # ---- Bootstrap ----

  @ACC-001 @release-0.2 @local
  Scenario: The first open creates the default environment and returns the bootstrap key
    When the engine is opened
    Then exactly one key is returned
    And with it, whoami names scopes "read", "write" and "admin" and environment "default/default"
    And listing environments shows one environment, named "default"

  @ACC-002 @release-0.2
  Scenario: The bootstrap key says who it is
    Given a client with the bootstrap key
    When the client asks whoami
    Then the answer names the key's ID, scopes "read", "write" and "admin", environment "default/default" and no expiry

  @ACC-003 @release-0.2 @local
  Scenario: A second open creates nothing and returns no key
    Given the engine was opened and returned its bootstrap key
    When the engine is closed and opened again
    Then no key is returned
    And the bootstrap key still authenticates
    And listing environments still shows one environment, with the same ID

  @ACC-004 @release-0.2 @local
  Scenario: A seed becomes the bootstrap key and no key is returned
    When the engine is opened with a seed key
    Then no key is returned and the open reports the seed as current
    And the seed key authenticates with scopes "read", "write" and "admin"

  @ACC-005 @release-0.2 @local
  Scenario: A seed with a new key ID rotates the seeded key
    Given the engine was opened with seed key A
    When the engine is reopened with seed key B, which has another key ID
    Then key B authenticates as an admin key
    And key A is refused as an invalid key

  @ACC-006 @release-0.2 @local
  Scenario Outline: Reopening with the same seed, or none, changes nothing
    Given the engine was opened with seed key A
    When the engine is reopened with <seed>
    Then key A still authenticates
    And no key is returned

    Examples:
      | seed          |
      | seed key A    |
      | no seed       |

  @ACC-007 @release-0.2 @local
  Scenario Outline: A malformed seed is refused and not echoed
    When the engine is opened with <seed> as the seed
    Then the open is refused and names the key format
    And the error does not contain the seed

    Examples:
      | seed                                    |
      | "not-a-key"                             |
      | a valid key with one character removed  |
      | a valid key whose ID is in uppercase    |
      | a valid key whose ID is not a UUIDv7    |

  @ACC-008 @release-0.2 @local
  Scenario Outline: A seed whose key ID is already a key is refused
    Given the engine was opened with seed key A
    And <history>
    When the engine is reopened with <seed>
    Then the open is refused as a seed conflict
    When the engine is reopened with no seed
    Then key A still authenticates

    Examples:
      | history                                          | seed                                          |
      | a client with key A created a key K              | a seed with K's key ID                        |
      | nothing else happened                            | a seed with A's key ID and another secret     |

  @ACC-014 @release-0.2 @local
  Scenario Outline: Reopening with a revoked seed opens and keeps it revoked
    Given the engine was opened with seed key A
    And a client with key A created an admin key K
    And <history>
    When the engine is reopened with seed key A
    Then the open succeeds and reports the seed as revoked
    And key A is refused as an invalid key
    And key K still authenticates

    Examples:
      | history                                          |
      | a client with key K revoked key A                |
      | the engine was reopened with seed key B          |

  @ACC-009 @release-0.2 @local
  Scenario: Two installs get different default IDs
    When two engines are opened in two fresh directories
    Then their environments have different IDs and different project IDs
    And both environments have the address "default/default"

  @ACC-010 @release-0.2
  Scenario: A key lists the environment it reaches
    Given a client with a "read" key
    When the client lists environments
    Then the list holds one environment, named "default", with its ID, project ID, address "default/default" and creation time

  @ACC-011 @release-0.2 @local
  Scenario: A lost admin key is recovered from the data directory
    Given a client with the bootstrap key revoked the bootstrap key
    When an admin key is recovered from the engine directory
    Then the recovered key authenticates with scopes "read", "write" and "admin"
    And listing keys shows it with origin "recovered" and label "recovered"
    And the revoked bootstrap key is still refused

  @ACC-012 @release-0.2 @local
  Scenario: Recovery needs a bootstrapped engine
    When an admin key is recovered from the fresh engine directory
    Then the recovery is refused as not ready
    And the directory is still empty

  @ACC-013 @release-0.2 @local
  Scenario: A seed added later leaves the bootstrap key alone
    Given the engine was opened and returned its bootstrap key
    When the engine is reopened with seed key A
    Then key A authenticates as an admin key
    And the bootstrap key still authenticates

  @ACC-016 @release-0.2 @local
  Scenario Outline: A lost control plane or ledger is refused, not replaced
    Given the engine was opened, a client emitted event "order.placed", and the engine was closed
    When <loss>
    And the engine is opened again
    Then the open is refused as <refusal>
    And no key is returned, and nothing in the directory changed but the lock file

    Examples:
      | loss                                             | refusal          |
      | the control plane file is deleted                | orphaned         |
      | the control plane file and the lock file are deleted | orphaned     |
      | the environment's ledger directory is deleted    | ledger missing   |
      | the environment's ledger directory is emptied    | ledger missing   |

  @ACC-017 @release-0.2 @local
  Scenario: A ledger directory is not an engine directory
    Given a ledger directory written by the ledger alone
    When an engine is opened on that directory
    Then the open is refused as not an engine directory
    And nothing in the directory changed

  @ACC-018 @release-0.2 @local
  Scenario: A second engine on the same directory changes nothing
    Given an engine is open on the directory with seed key A
    When a second engine is opened on the same directory with seed key B
    Then the second open is refused as locked
    And key A still authenticates on the first engine
    And key B is refused on the first engine

  # ---- Authentication ----

  @ACC-020 @release-0.2
  Scenario Outline: A call without a key is unauthenticated
    When a client with <key> lists events
    Then the call is refused as unauthenticated with "authentication required"
    And the error is not retryable

    Examples:
      | key           |
      | no key        |
      | an empty key  |

  @ACC-021 @release-0.2
  Scenario Outline: An unknown key or a wrong secret is refused with no hint
    When a client presenting <key> lists events
    Then the call is refused as unauthenticated with "invalid API key"
    And the error has no hint

    Examples:
      | key                                          |
      | a well-formed key no engine minted           |
      | the bootstrap key's ID with another secret       |

  @ACC-024 @release-0.2 @local
  Scenario: A key minted by another engine is refused like an unknown key
    Given a second engine opened in another fresh directory, and its bootstrap key
    When a client presenting that key lists events on the first engine
    Then the call is refused as unauthenticated with "invalid API key"
    And the error has no hint

  @ACC-022 @release-0.2
  Scenario Outline: A malformed key is refused with a hint about the format
    When a client presenting <key> lists events
    Then the call is refused as unauthenticated with "invalid API key"
    And the hint names the key format
    And the error does not contain the presented key

    Examples:
      | key                                      |
      | "bad-key"                                |
      | the bootstrap key with a "Bearer " prefix    |
      | the bootstrap key with one character removed |

  @ACC-023 @release-0.2
  Scenario: A valid key acts in its own environment
    Given a client with a "write" key
    And another client with a "read" key
    When the first client emits event "order.placed"
    And the second client reads that event by ID
    Then the call succeeds
    And whoami with either key names environment "default/default"

  # ---- Scopes ----

  @ACC-030 @release-0.2
  Scenario Outline: Each operation needs its scope
    Given an event "order.placed" was emitted with the bootstrap key
    And a client with a key holding only "<scope>"
    When the client calls <operation>
    Then the call <result>

    Examples:
      | scope | operation           | result                                                      |
      | read  | list events         | succeeds                                                    |
      | read  | get the event       | succeeds                                                    |
      | read  | list environments   | succeeds                                                    |
      | read  | emit                | is refused as permission denied with "missing scope: write" |
      | read  | create a key        | is refused as permission denied with "missing scope: admin" |
      | read  | list keys           | is refused as permission denied with "missing scope: admin" |
      | write | emit                | succeeds                                                    |
      | write | emit a batch        | succeeds                                                    |
      | write | list events         | is refused as permission denied with "missing scope: read"  |
      | write | revoke a key        | is refused as permission denied with "missing scope: admin" |
      | admin | list keys           | succeeds                                                    |
      | admin | emit                | is refused as permission denied with "missing scope: write" |
      | admin | list events         | is refused as permission denied with "missing scope: read"  |

  @ACC-031 @release-0.2
  Scenario: A missing scope is named in the hint
    Given a client with a "read" key
    When the client emits event "order.placed"
    Then the call is refused as permission denied
    And the hint says a key with scope "write" is needed
    And the error is not retryable
    And no event is stored

  @ACC-032 @release-0.2
  Scenario: Any valid key may ask whoami
    Given a client with a "read" key created with label "agent-7" and an expiry
    When the client asks whoami
    Then the answer names the key's ID, scope "read", environment "default/default" and the expiry

  # ---- Creating keys ----

  @ACC-040 @release-0.2
  Scenario: A new key is read-only unless asked otherwise, and its secret is shown once
    Given a client with the bootstrap key
    When the client creates a key without scopes
    Then the answer holds the key and its scopes are "read"
    And listing keys shows the key without its secret

  @ACC-041 @release-0.2
  Scenario: A key is created with chosen scopes, a label and an expiry
    Given a client with the bootstrap key
    When the client creates a key with scopes "read" and "write", label "ci" and an expiry in one hour
    Then a client with the new key can emit and list events
    And listing keys shows it with label "ci", those scopes, the expiry, status "active", origin "created" and the bootstrap key as its creator

  @ACC-042 @release-0.2
  Scenario: A key cannot grant a scope it does not hold
    Given a client with a key holding only "admin"
    When the client creates a key with scope "write"
    Then the call is refused as invalid
    And no key is created

  @ACC-043 @release-0.2
  Scenario Outline: A bad key request is refused
    Given a client with the bootstrap key
    When the client creates a key with <request>
    Then the call is refused as invalid
    And no key is created

    Examples:
      | request                                 |
      | an expiry an hour ago                   |
      | a label of 129 bytes                    |
      | a label holding a line break            |

  @ACC-046 @release-0.2 @local
  Scenario: An expiry at the engine's now is refused
    Given a client with the bootstrap key and the clock reading T
    When the client creates a key expiring at T
    Then the call is refused as invalid
    And no key is created

  @ACC-047 @release-0.2
  Scenario: Keys are listed a page at a time, newest first
    Given a client with the bootstrap key created keys A, B and C, in that order
    When the client lists keys 2 at a time
    Then the first page holds C and B and names a next page
    And the next page holds A and the bootstrap key, and names none

  @ACC-044 @release-0.2 @local
  Scenario: Listing keys shows each key's status
    Given a client with the bootstrap key created keys A, B and C, with C expiring in one hour
    And the client revoked key B
    And two hours pass
    When the client lists keys
    Then A is "active", B is "revoked" and C is "expired"
    And no listed key holds a secret

  @ACC-045 @release-0.2
  Scenario Outline: A key cannot outlive its creator
    Given a client with an "admin" and "read" key expiring in one hour
    When the client creates a key with scope "read" and <expiry>
    Then the call <result>

    Examples:
      | expiry                                           | result                  |
      | no expiry                                        | is refused as invalid   |
      | an expiry one millisecond after the creator's    | is refused as invalid   |
      | an expiry equal to the creator's                 | succeeds                |
      | an expiry in 30 minutes                          | succeeds                |

  # ---- Revoking and expiry ----

  @ACC-050 @release-0.2
  Scenario: A revoked key is refused on its next call, like an unknown key
    Given a client with a "read" key K that has listed events
    When a client with the bootstrap key revokes K
    And the client with K lists events again
    Then the call is refused as unauthenticated with "invalid API key"
    And the error has no hint

  @ACC-051 @release-0.2
  Scenario: Revoking a revoked key succeeds; revoking an unknown one is not found
    Given a client with the bootstrap key revoked key K
    When the client revokes K again
    Then the call succeeds
    When the client revokes a key ID no key has
    Then the call is refused as not found with "key not found"

  @ACC-052 @release-0.2
  Scenario: A key may revoke itself, the bootstrap key included
    Given a client with the bootstrap key
    When the client revokes the bootstrap key
    Then the call succeeds
    And the client's next call is refused as unauthenticated

  @ACC-053 @release-0.2 @local
  Scenario: A key works until its expiry, then is refused like an unknown key
    Given a client with a "read" key expiring at time T
    When the clock reads one millisecond before T
    And the client lists events
    Then the call succeeds
    When the clock reads T
    And the client lists events
    Then the call is refused as unauthenticated with "invalid API key"
    And the error has no hint

  @ACC-054 @release-0.2 @local
  Scenario: An expired key stays refused when the clock steps back, across a restart
    Given a client with a "read" key expiring at time T
    And the clock reads T and the key was refused
    When the clock steps back one hour
    And the engine is closed and opened again
    Then the client's next call is refused as unauthenticated with "invalid API key"

  # ---- Who emitted ----

  @ACC-060 @release-0.2
  Scenario: Every event records the key that emitted it
    Given a client with a "write" and "read" key K
    When the client emits event "order.placed"
    Then reading the event shows emitted_by "key:" followed by K's ID

  @ACC-061 @release-0.2
  Scenario: Every event of a batch records the same emitter
    Given a client with a "write" and "read" key K
    When the client emits a batch of 3 events
    Then each of the 3 events shows emitted_by "key:" followed by K's ID
