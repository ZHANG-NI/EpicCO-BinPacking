# AlphaCO: Two-Dimensional Guillotine Bin Packing

The solver that [AlphaCO](https://github.com/ZHANG-NI/AlphaCO) produced for the two-dimensional guillotine bin packing problem, starting from GDRR (J. Gardeyn and T. Wauters, European Journal of Operational Research, 2022; https://github.com/JeroenGar/gdrr-2bp).

## Run

```
mkdir -p bin
bash build.sh bin
bin/solver <instance> <solution> -t <seconds> -seed <seed>
```

`build.sh` needs a Rust toolchain and builds offline: it expects the crates vendored (`cargo vendor`) in `<toolchain>/vendor_gdrr`.
