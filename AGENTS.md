# Agent instructions

- [CONTRIBUTING.md](CONTRIBUTING.md) owns the branch model, the validation
  commands and the commit style. Follow it.
- [docs/INVARIANTS.md](docs/INVARIANTS.md) and section 2 of
  [docs/IME.md](docs/IME.md#2-behavioural-specification) are binding.
  Read the sections that cover the code you change before you change it.
- [docs/DESIGN.md](docs/DESIGN.md) explains the structure and points to the
  owner of each concern; [HANDOVER.md](HANDOVER.md) holds the current state
  and open issues.
- Never restart or stop a running Sommelier service to test a change; see
  [Manual verification](docs/INVARIANTS.md#manual-verification).
