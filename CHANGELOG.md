# Changelog

Each release lists what it adds and changes. The binary carries foundation's release number until 1.0.0. Releases up to 0.3.0 were built in foundation; their notes are in [foundation's changelog](https://github.com/ironflowx/foundation/blob/main/CHANGELOG.md).

## Unreleased

- **The `execute` scope and the `conflict` kind** ([foundation#218](https://github.com/ironflowx/foundation/issues/218), [#5](https://github.com/ironflowx/ironflow/issues/5)): foundation pinned at `e20948dd7b93`. `ironflowx keys create --scopes execute` makes a key that holds `execute` alone, which no operation needs yet; a new data directory's admin key, a seed and a recovered key hold `read`, `write`, `admin` and `execute`. A data directory written by 0.3 is refused at start, its log and its `control.db` being of older formats. `describe` lists exit code 9, `conflict`, not retryable, which no call answers yet.
- **The binary moves here from foundation** ([#2](https://github.com/ironflowx/ironflow/issues/2)): the `ironflowx` binary and CLI, their tests and behavior scenarios, the server crash test and the release archives are built from this repository, on foundation pinned at `d840c6cb611d`. No behavior change.
