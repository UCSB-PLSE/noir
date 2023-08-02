cfg_if::cfg_if! {
    if #[cfg(feature = "axiom_halo2_backend")] {
        pub(crate) use noir_halo2_backend_axiom::AxiomHalo2 as ConcreteBackend;
    } else if #[cfg(feature = "pse_halo2_backend")] {
        pub(crate) use noir_halo2_backend_pse::PseHalo2 as ConcreteBackend;
    } else {
        pub(crate) use acvm_backend_barretenberg::Barretenberg as ConcreteBackend;
    }
}
#[cfg(not(any(
    feature = "plonk_bn254",
    feature = "plonk_bn254_wasm",
    feature = "axiom_halo2_backend",
    feature = "pse_halo2_backend",
)))]
compile_error!("please specify a backend to compile with");

#[cfg(all(feature = "plonk_bn254", feature = "plonk_bn254_wasm"))]
compile_error!(
    "feature \"plonk_bn254\"  and feature \"plonk_bn254_wasm\" cannot be enabled at the same time"
);
