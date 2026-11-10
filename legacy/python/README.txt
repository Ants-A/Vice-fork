Pre-Rust Vice implementation and behavioral reference tests.

The supported runtime is the Rust crate at the repository root. These sources
are retained for migration review and behavioral comparisons; no build, launcher,
service or installer invokes them. Resources (UI, data and fonts) remain under
vice/ and are shared by the Rust implementation.
