# Extensionalities isolation contract

- Keep fork-specific source, manifests, lockfile, fixtures, and documentation inside this directory.
- This is an independent Cargo workspace. Do not register its crates in the upstream root workspace.
- Use the upstream `liteparse` public library through the path dependency. Do not patch its CLI or other upstream-owned files.
- Build and check with the explicit `extensionalities/Cargo.toml` manifest; commit this workspace's `Cargo.lock`.
- Keep built-in Tesseract enabled. Do not disable it to make a build pass.
- `litx` is a scaffold, not an upstream `lit` replacement. CLI compatibility and extraction features require separate approval.
- Preserve the root workspace and lockfile during dependency updates. Public API changes can break this workspace even when Git merges cleanly.
- Keep build products, credentials, and private documents out of commits. Review fixtures before adding them.
- Do not push, publish, activate delivery bundles, or modify consumer recipes without explicit authorization.
