## Summary

<!-- What and why? -->

## Validation

- [ ] `cargo fmt --check`
- [ ] `cargo test --locked`
- [ ] `cargo clippy --all-targets --locked -- -D warnings`
- [ ] `./scripts/integration_test.py --quick`
- [ ] Performance measurements included, if relevant

## Security / compatibility

- [ ] No token, endpoint, index, log, or private path was added
- [ ] IPC/version/schema compatibility impact documented
