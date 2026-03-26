# How to Contribute to this Repository

We value contributions from the community and will do everything we
can to get them reviewed in a timely fashion. If you have code to send
our way or a bug to report:

* **Contributing Code**: If you have new code or a bug fix, fork this
  repo, create a logically-named branch, and [submit a PR against this
  repo](https://github.com/helium/multi-gateway). Include a write up
  of the PR with details on what it does. For non-trivial changes,
  please open an issue first to discuss the approach.

* **Reporting Bugs**: Open an issue [against this
  repo](https://github.com/helium/multi-gateway/issues) with as much
  detail as you can. At the very least, include steps to reproduce
  the problem.

## Development

This project is written in Rust. Make sure you have a recent stable
toolchain installed (see `rust-toolchain.toml` for the pinned version
used in CI). To build and run the tests locally:

```sh
cargo build
cargo test
```

CI runs on every pull request — please ensure `cargo clippy` and
`cargo fmt --check` pass before submitting.

## Code of Conduct

This project is intended to be a safe, welcoming space for
collaboration, and contributors are expected to adhere to the
[Contributor Covenant Code of
Conduct](http://contributor-covenant.org/).

Above all, thank you for taking the time to be a part of the Helium community.
