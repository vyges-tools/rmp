# vyges-rmp

Logic restructuring on an AIG: re-synthesis of timing-critical or area-heavy logic, with every rewrite checkable for equivalence.

> **Status: in development.** `restructure` (area and timing targets), `resynth` (delay re-mapping of the
> logic behind every failing end point), `resynth-annealing` and `resynth-genetic` (searches over AIG
> rewrite sequences for the best worst slack), `blif-write` and `blif-read` are built;
> `run` refuses with exit 3. Commands are enabled one at a time as
> each is validated.

## Why this exists

Restructuring rebuilds logic for area or delay after placement. Built on one AIG and SAT core shared with the formal engine, every rewrite can be proved equivalent to what it replaced.

## Usage

```
vyges physical rmp restructure <design.odb> --liberty FILE --sdc FILE --liberty-file FILE \
    --tielo-port CELL/PORT --tiehi-port CELL/PORT -o OUT.odb
vyges physical rmp resynth <design.odb> --liberty [CORNER=]FILE... --sdc FILE \
    [--corners A,B --corner A] [--dont-use PATTERN...] -o OUT.odb
vyges physical rmp resynth-annealing <design.odb> (resynth's options) \
    [--iters N] [--revert-after N] [--seed S] [--temp T] [--initial-ops N] [--slack-threshold S]
vyges physical rmp resynth-genetic <design.odb> (resynth's options) \
    [--iters N] [--seed S] [--population-size N] [--mutation-probability P] [--crossover-probability P] \
    [--tournament-size N] [--tournament-probability P] [--initial-ops N] [--slack-threshold S]
vyges physical rmp blif-write <design.odb> --liberty FILE --inst NAME... -o OUT.blif
vyges physical rmp blif-read  <design.odb> --liberty FILE --inst NAME... --blif FILE -o OUT.odb
vyges physical rmp run <design.odb> <job> -o OUT.odb
vyges physical rmp --describe
```

## Events

Every command leaves a causal trail of [`vyges-events`](https://github.com/vyges-tools/events) JSON
lines on **stderr**; reports and log lines go to stdout or `-o`, so either can be parsed without the other.

| code | meaning |
|---|---|
| `RMP-INPUT` | an input could not be read or an output written (exit 2) |
| `RMP-REFUSED` | the command, or what it was asked to do, is not implemented in this build (exit 3) |
| `RMP-STEP` | one step of the command, at `debug` |
| `RMP-DONE` | every command: its status (`ok`, `finding`, `input`, `refused`) and exit code |

`-q` / `--quiet` raises the trail's floor to `error`; `-v` / `--verbose` lowers it to `debug`.
An explicit `VYGES_LOG` in the environment wins over both.

## Install

```
vyges install physical
```

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
