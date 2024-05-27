use ring::rand;
use ring::signature::{Ed25519KeyPair};

pub fn generate_key_pair() -> (Vec<u8>, Vec<u8>) {
    let rng = rand::SystemRandom::new();

    // Generate a key pair
    let pkcs8_bytes = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8_bytes.as_ref()).unwrap();

    // Extract the private key
    let private_key = pkcs8_bytes.as_ref().to_vec();

    const MESSAGE: &[u8] = b"hello, world";
    key_pair.sign(MESSAGE);


    // Extract the public key
    let public_key = key_pair.public_key().as_ref().to_vec();

    return (private_key, public_key);
}
