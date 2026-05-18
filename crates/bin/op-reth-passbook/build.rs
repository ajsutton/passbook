//! Emits `VERGEN_CARGO_FEATURES` for THIS binary crate's enabled cargo
//! features so `reth_info{cargo_features=…}` reflects the actual build
//! (e.g. `jemalloc` in the Docker/release build). Without this the
//! custom binary inherits reth-node-core's empty compile-time value.
use vergen::{CargoBuilder, Emitter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut emitter = Emitter::default();
    emitter.add_instructions(&CargoBuilder::default().features(true).build()?)?;
    emitter.emit()?;
    Ok(())
}
