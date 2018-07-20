# lazy-pregen &emsp; [![Latest Version][crates.io]]

[Latest Version]: https://img.shields.io/crates/v/lazy-pregen.svg
[crates.io]: https://crates.io/crates/lazy-pregen

A Rust command line Minecraft Server wrapper used to pregenerate chunks.

[Documentation](https://docs.rs/lazy-pregen/)

lazy-pregen was made for personal use and thus has several issues that the
author did not care to deal with. It is highly recommended to read the above
documentation link.

## Usage

Run the following command:

```
cargo install lazy-pregen
```

and then, assuming your PATH variable is set up properly, run the pregenerator:

```
lazy-pregen [DIRECTORY]
```

If no directory is specified, it defaults to the current working directory.