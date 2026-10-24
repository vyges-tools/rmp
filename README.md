# vyges-rmp

Logic restructuring on an AIG: re-synthesis of timing-critical or area-heavy logic, with every rewrite checkable for equivalence.

> **Status: in development.** Every command currently refuses with exit 3. Commands are enabled one
> at a time as each is validated.

## Why this exists

Restructuring rebuilds logic for area or delay after placement. Built on one AIG and SAT core shared with the formal engine, every rewrite can be proved equivalent to what it replaced.

## Usage

```
vyges physical rmp run <design.odb> <job> -o OUT.odb
vyges physical rmp --describe
```

## Install

```
vyges install physical
```

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
