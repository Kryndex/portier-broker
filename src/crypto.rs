extern crate openssl;
extern crate rand;
extern crate rustc_serialize;

use emailaddress::EmailAddress;
use self::openssl::bn::BigNum;
use self::openssl::crypto::hash;
use self::openssl::crypto::pkey::PKey;
use self::openssl::crypto::rsa::RSA;
use self::rand::{OsRng, Rng};
use self::rustc_serialize::base64::{self, FromBase64, ToBase64};
use serde_json::de::from_slice;
use serde_json::value::Value;
use std;
use std::fs::File;
use std::io::{Read, Write};


/// Union of all possible error types seen while parsing.
#[derive(Debug)]
pub enum CryptoError {
    Custom(&'static str),
    Io(std::io::Error),
    Ssl(openssl::ssl::error::SslError),
}

impl From<&'static str> for CryptoError {
    fn from(err: &'static str) -> CryptoError {
        CryptoError::Custom(err)
    }
}

impl From<std::io::Error> for CryptoError {
    fn from(err: std::io::Error) -> CryptoError {
        CryptoError::Io(err)
    }
}

impl From<openssl::ssl::error::SslError> for CryptoError {
    fn from(err: openssl::ssl::error::SslError) -> CryptoError {
        CryptoError::Ssl(err)
    }
}


/// A named key pair, for use in JWS signing.
#[derive(Clone)]
pub struct NamedKey {
    id: String,
    key: PKey,
}


impl NamedKey {
    /// Creates a NamedKey by reading a `file` path and generating an `id`.
    pub fn from_file(filename: &str) -> Result<NamedKey, CryptoError> {
        let mut file = File::open(filename)?;
        let mut file_contents = String::new();
        file.read_to_string(&mut file_contents)?;

        NamedKey::from_pem_str(&file_contents)
    }

    /// Creates a NamedKey from a PEM-encoded str.
    pub fn from_pem_str(pem: &str) -> Result<NamedKey, CryptoError> {
        let pkey = PKey::private_key_from_pem(&mut pem.as_bytes())?;

        NamedKey::from_pkey(pkey)
    }

    /// Creates a NamedKey from a PKey
    pub fn from_pkey(pkey: PKey) -> Result<NamedKey,CryptoError> {
        let e = pkey.get_rsa().e().expect("unable to retrieve key's e value");
        let n = pkey.get_rsa().n().expect("unable to retrieve key's n value");

        let mut hasher = hash::Hasher::new(hash::Type::SHA256);
        hasher.write_all(e.to_vec().as_slice()).expect("pubkey hashing failed");
        hasher.write_all(b".").expect("pubkey hashing failed");
        hasher.write_all(n.to_vec().as_slice()).expect("pubkey hashing failed");
        let name = hasher.finish().to_base64(base64::URL_SAFE);

        Ok(NamedKey { id: name, key: pkey })
    }

    /// Create a JSON Web Signature (JWS) for the given JSON structure.
    pub fn sign_jws(&self, payload: &Value) -> String {
        let header = json!({
            "kid": &self.id,
            "alg": "RS256",
        }).to_string();

        let payload = payload.to_string();
        let mut input = Vec::<u8>::new();
        input.extend(header.as_bytes().to_base64(base64::URL_SAFE).into_bytes());
        input.push(b'.');
        input.extend(payload.as_bytes().to_base64(base64::URL_SAFE).into_bytes());

        let sha256 = hash::hash(hash::Type::SHA256, &input);
        let sig = self.key.sign(&sha256);
        input.push(b'.');
        input.extend(sig.to_base64(base64::URL_SAFE).into_bytes());
        String::from_utf8(input).expect("unable to coerce jwt into string")
    }

    /// Return JSON represenation of the public key for use in JWK key sets.
    pub fn public_jwk(&self) -> Value {
        fn json_big_num(n: &BigNum) -> String {
            n.to_vec().to_base64(base64::URL_SAFE)
        }
        let n = self.key.get_rsa().n().expect("unable to retrieve key's n value");
        let e = self.key.get_rsa().e().expect("unable to retrieve key's e value");
        json!({
            "kty": "RSA",
            "alg": "RS256",
            "use": "sig",
            "kid": &self.id,
            "n": json_big_num(&n),
            "e": json_big_num(&e),
        })
    }
}


/// Helper function to build a session ID for a login attempt.
///
/// Put the email address, the client ID (RP origin) and some randomness into
/// a SHA256 hash, and encode it with URL-safe bas64 encoding. This is used
/// as the key in Redis, as well as the state for OAuth authentication.
pub fn session_id(email: &EmailAddress, client_id: &str) -> String {
    let mut rng = OsRng::new().expect("unable to create rng");
    let rand_bytes: Vec<u8> = (0..16).map(|_| rng.gen()).collect();

    let mut hasher = hash::Hasher::new(hash::Type::SHA256);
    hasher.write_all(email.to_string().as_bytes()).expect("session hashing failed");
    hasher.write_all(client_id.as_bytes()).expect("session hashing failed");
    hasher.write_all(&rand_bytes).expect("session hashing failed");
    hasher.finish().to_base64(base64::URL_SAFE)
}


pub fn nonce() -> String {
    let mut rng = OsRng::new().expect("unable to create rng");
    let rand_bytes: Vec<u8> = (0..16).map(|_| rng.gen()).collect();
    rand_bytes.to_base64(base64::URL_SAFE)
}


/// Helper function to deserialize key from JWK Key Set.
///
/// Searches the provided JWK Key Set Value for the key matching the given
/// id. Returns a usable public key if exactly one key is found.
pub fn jwk_key_set_find(set: &Value, kid: &str) -> Result<PKey, ()> {
    let key_objs = set.get("keys").and_then(|v| v.as_array()).ok_or(())?;
    let matching = key_objs.iter()
        .filter(|key_obj| {
            key_obj.get("kid").and_then(|v| v.as_str()) == Some(kid) &&
            key_obj.get("use").and_then(|v| v.as_str()) == Some("sig")
        })
        .collect::<Vec<&Value>>();

    // Verify that we found exactly one key matching the key ID.
    if matching.len() != 1 {
        return Err(());
    }

    // Then, use the data to build a public key object for verification.
    let n = matching[0].get("n").and_then(|v| v.as_str()).ok_or(())
                .and_then(|data| data.from_base64().map_err(|_| ()))
                .and_then(|data| BigNum::new_from_slice(&data).map_err(|_| ()))?;
    let e = matching[0].get("e").and_then(|v| v.as_str()).ok_or(())
                .and_then(|data| data.from_base64().map_err(|_| ()))
                .and_then(|data| BigNum::new_from_slice(&data).map_err(|_| ()))?;
    let rsa = RSA::from_public_components(n, e).map_err(|_| ())?;
    let mut pub_key = PKey::new();
    pub_key.set_rsa(&rsa);
    Ok(pub_key)
}


/// Verify a JWS signature, returning the payload as Value if successful.
pub fn verify_jws(jws: &str, key_set: &Value) -> Result<Value, ()> {
    // Extract the header from the JWT structure. Determine what key was used
    // to sign the token, so we can then verify the signature.
    let parts: Vec<&str> = jws.split('.').collect();
    if parts.len() != 3 {
        return Err(());
    }
    let decoded = parts.iter().map(|s| s.from_base64())
                    .collect::<Result<Vec<_>, _>>().map_err(|_| ())?;
    let jwt_header: Value = from_slice(&decoded[0]).map_err(|_| ())?;
    let kid = jwt_header.get("kid").and_then(|v| v.as_str()).ok_or(())?;
    let pub_key = jwk_key_set_find(key_set, kid)?;

    // Verify the identity token's signature.
    let message_len = parts[0].len() + parts[1].len() + 1;
    let sha256 = hash::hash(hash::Type::SHA256, jws[..message_len].as_bytes());
    if !pub_key.verify(&sha256, &decoded[2]) {
        return Err(());
    }

    Ok(from_slice(&decoded[1]).map_err(|_| ())?)
}
