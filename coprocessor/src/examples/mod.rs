use alloy::dyn_abi::SolType;
use coprocessor_circuit_types::CoprocessorCircuitInputs;
use sha2::{Digest, Sha256};
use sp1_helios_primitives::types::ProofOutputs;
use sp1_sdk::{HashableKey, ProverClient, SP1Stdin};
use sp1_verifier::Groth16Verifier;
use tendermint_program_types::TendermintOutput;

use crate::{
    COPROCESSOR_CIRCUIT_ELF,
    constants::{ETHEREUM_HEIGHT_KEY, ETHEREUM_ROOT_KEY, NEUTRON_HEIGHT_KEY, NEUTRON_ROOT_KEY},
    coprocessor::Coprocessor,
    lightclients::{helios::SP1HeliosOperator, tendermint::SP1TendermintOperator},
};

#[cfg(feature = "mailbox")]
pub mod mailbox;

pub async fn prove_coprocessor(coprocessor: &mut Coprocessor) -> (TendermintOutput, ProofOutputs) {
    let mut ethereum_operator = SP1HeliosOperator::new();
    // todo: remove hardcoded ethereum height and replace it with a real trusted height
    // in production we take our last trusted slot and calculate the current period from the last finalized slot.
    // then we request the update, generate the proof and verify it against the active sync committee.
    // if the proof is valid for that committee and the slot height is greater than the last trusted slot,
    // we update the trusted height and root, as well as the sync committee (if it changed)
    let ethereum_light_client_proof = ethereum_operator.run(7553024 - (32 * 8192), 33).await;
    let ethereum_light_client_proof = ethereum_light_client_proof.unwrap().unwrap();
    let helios_proof_serialized = ethereum_light_client_proof.bytes();
    let helios_public_values = ethereum_light_client_proof.public_values.to_vec();
    let helios_vk = ethereum_operator.get_vk();
    let neutron_operator = SP1TendermintOperator::new(
        coprocessor.trusted_neutron_height,
        coprocessor.target_neutron_height,
    );
    let neutron_light_client_proof = neutron_operator.run().await;
    let neutron_proof_serialized = neutron_light_client_proof.bytes();
    let neutron_public_values = neutron_light_client_proof.public_values.to_vec();
    let neutron_vk = neutron_operator.get_vk();
    let neutron_output: TendermintOutput =
        serde_json::from_slice(&neutron_light_client_proof.public_values.to_vec()).unwrap();
    let helios_output: ProofOutputs =
        ProofOutputs::abi_decode(&ethereum_light_client_proof.public_values.to_vec(), false)
            .unwrap();
    let target_neutron_root: Vec<u8> = neutron_output.target_header_hash.to_vec();
    let target_ethereum_root: Vec<u8> = helios_output.newHeader.to_vec();
    let target_neutron_height: u64 = neutron_output.target_height;
    let target_ethereum_height: u64 = helios_output.newHead.try_into().unwrap();
    let mut coprocessor_root = coprocessor.smt_root;
    let mut hasher = Sha256::new();
    hasher.update(NEUTRON_HEIGHT_KEY);
    let neutron_height_key = hasher.finalize();
    let mut hasher = Sha256::new();
    hasher.update(ETHEREUM_HEIGHT_KEY);
    let ethereum_height_key = hasher.finalize();
    let mut hasher = Sha256::new();
    hasher.update(NEUTRON_ROOT_KEY);
    let neutron_root_key = hasher.finalize();
    let mut hasher = Sha256::new();
    hasher.update(ETHEREUM_ROOT_KEY);
    let ethereum_root_key = hasher.finalize();
    coprocessor_root = coprocessor
        .smt_tree
        .insert(
            coprocessor_root,
            "demo",
            &neutron_height_key,
            target_neutron_height.to_be_bytes().to_vec(),
        )
        .expect("Failed to insert Neutron Height");
    coprocessor_root = coprocessor
        .smt_tree
        .insert(
            coprocessor_root,
            "demo",
            &ethereum_height_key,
            target_ethereum_height.to_be_bytes().to_vec(),
        )
        .expect("Failed to insert Ethereum Height");
    coprocessor_root = coprocessor
        .smt_tree
        .insert(
            coprocessor_root,
            "demo",
            &neutron_root_key,
            target_neutron_root.clone(),
        )
        .expect("Failed to insert Ethereum Root");
    coprocessor_root = coprocessor
        .smt_tree
        .insert(
            coprocessor_root,
            "demo",
            &ethereum_root_key,
            target_ethereum_root.clone(),
        )
        .expect("Failed to insert Ethereum Root");
    coprocessor.smt_root = coprocessor_root;
    let neutron_height_opening = coprocessor
        .smt_tree
        .get_opening("demo", coprocessor_root, &neutron_height_key)
        .unwrap()
        .unwrap();
    let ethereum_height_opening = coprocessor
        .smt_tree
        .get_opening("demo", coprocessor_root, &ethereum_height_key)
        .unwrap()
        .unwrap();
    let neutron_root_opening = coprocessor
        .smt_tree
        .get_opening("demo", coprocessor_root, &neutron_root_key)
        .unwrap()
        .unwrap();
    let ethereum_root_opening = coprocessor
        .smt_tree
        .get_opening("demo", coprocessor_root, &ethereum_root_key)
        .unwrap()
        .unwrap();
    coprocessor.trusted_neutron_height = neutron_output.trusted_height;
    coprocessor.trusted_ethereum_height = helios_output.prevHead.try_into().unwrap();
    coprocessor.trusted_neutron_root = neutron_output.target_header_hash.to_vec();
    coprocessor.trusted_ethereum_root = helios_output.prevHeader.to_vec();
    let coprocessor_inputs = CoprocessorCircuitInputs {
        helios_proof: helios_proof_serialized,
        helios_public_values,
        helios_vk,
        neutron_proof: neutron_proof_serialized,
        neutron_public_values,
        neutron_vk,
        previous_neutron_height: coprocessor.trusted_neutron_height,
        previous_ethereum_height: coprocessor.trusted_ethereum_height,
        previous_neutron_root: coprocessor.trusted_neutron_root.to_vec(),
        previous_ethereum_root: coprocessor.trusted_ethereum_root.to_vec(),
        neutron_height_opening,
        ethereum_height_opening,
        neutron_root_opening,
        ethereum_root_opening,
        coprocessor_root,
    };
    let coprocessor_circuit_inputs_serialized = borsh::to_vec(&coprocessor_inputs).unwrap();
    let client = ProverClient::from_env();
    let mut stdin = SP1Stdin::new();
    stdin.write_vec(coprocessor_circuit_inputs_serialized);
    let (pk, vk) = client.setup(COPROCESSOR_CIRCUIT_ELF);
    // This is the coprocessor update proof
    // that contains the new roots from the zk light clients
    // all future proofs can be verified against the state in this smt
    let proof = client
        .prove(&pk, &stdin)
        .groth16()
        .run()
        .expect("Failed to prove");
    // This verification should happen on-chain
    // our co-processor must adapt the new state
    // We must serialize the circuit outputs so that the
    // target chain can understand them
    let groth16_vk = *sp1_verifier::GROTH16_VK_BYTES;
    Groth16Verifier::verify(
        &proof.bytes(),
        &proof.public_values.to_vec(),
        &vk.bytes32(),
        groth16_vk,
    )
    .unwrap();
    (neutron_output, helios_output)
}

#[test]
fn generate_constants() {
    let mut hasher = Sha256::new();
    hasher.update(NEUTRON_HEIGHT_KEY);
    let neutron_height_key = hasher.finalize();
    let mut hasher = Sha256::new();
    hasher.update(ETHEREUM_HEIGHT_KEY);
    let ethereum_height_key = hasher.finalize();
    let mut hasher = Sha256::new();
    hasher.update(NEUTRON_ROOT_KEY);
    let neutron_root_key = hasher.finalize();
    let mut hasher = Sha256::new();
    hasher.update(ETHEREUM_ROOT_KEY);
    let ethereum_root_key = hasher.finalize();
    println!("neutron_height_key: {:?}", neutron_height_key);
    println!("ethereum_height_key: {:?}", ethereum_height_key);
    println!("neutron_root_key: {:?}", neutron_root_key);
    println!("ethereum_root_key: {:?}", ethereum_root_key);
}
