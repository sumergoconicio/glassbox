// Module: glassbox-cli upstream bridge.
// File: upstream.rs.
// Responsibility: Include the build-generated upstream implementation without copied CLI definitions.
include!(concat!(env!("OUT_DIR"), "/glassbox-upstream.rs"));
