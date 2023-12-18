use crate::{end_timer, start_timer};
use acvm::acir::{circuit::Circuit, native_types::WitnessMap};
use acvm::ProofSystemCompiler;

pub fn prove_execution<B: ProofSystemCompiler>(
    backend: &B,
    common_reference_string: &[u8],
    circuit: &Circuit,
    solved_witness: WitnessMap,
    proving_key: &[u8],
) -> Result<Vec<u8>, B::Error> {
    let prove_time = start_timer!(|| "Proving");
    // TODO(#1569): update from not just accepting `false` once we get nargo to interop with dynamic backend
    let result =
        backend.prove_with_pk(common_reference_string, circuit, solved_witness, proving_key, false);
    end_timer!(prove_time);
    result
}
