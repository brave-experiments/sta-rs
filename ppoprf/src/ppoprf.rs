//! This module defines the `Client` and `Server` functionality for a
//! puncturable partially oblivious pseudorandom function (PPOPRF).
//!
//! The POPRF that is used is very similar to the design of [Tyagi et
//! al.](https://eprint.iacr.org/2021/864.pdf), but where H_3 is
//! replaced with a puncturable PRF evaluation (over a small input
//! domain). This allows puncturing metadata tags from PPOPRF server
//! secret keys, which in turn gives forward-security guarantees related
//! to the pseudorandomness of evaluations received by clients.
//!
//! This construction is primarily used in the STAR protocol for
//! providing secure randomness to clients.

extern crate rand;

extern crate rand_core;
use rand_core::RngCore;
use rand_core_ristretto::OsRng;

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;

use serde::{de, ser, Deserialize, Serialize};

use std::collections::HashMap;
use std::convert::TryInto;

use strobe_rng::StrobeRng;
use strobe_rs::{SecParam, Strobe};

pub use crate::PPRFError;
use crate::{ggm::GGM, PPRF};

pub const COMPRESSED_POINT_LEN: usize = 32;
pub const DIGEST_LEN: usize = 64;

#[derive(Serialize, Deserialize)]
pub struct ProofDLEQ {
    c: Scalar,
    s: Scalar,
}
impl ProofDLEQ {
    pub fn new(
        key: &Scalar,
        public_value: &RistrettoPoint,
        p: &RistrettoPoint,
        q: &RistrettoPoint,
    ) -> Self {
        let mut csprng = OsRng;
        let t = Scalar::random(&mut csprng);

        let tg = t * RISTRETTO_BASEPOINT_POINT;
        let tp = t * p;
        let chl = ProofDLEQ::hash(&[&RISTRETTO_BASEPOINT_POINT, public_value, p, q, &tg, &tp]);
        let s = t - (chl * key);
        Self { c: chl, s }
    }

    pub fn verify(
        &self,
        public_value: &RistrettoPoint,
        p: &RistrettoPoint,
        q: &RistrettoPoint,
    ) -> bool {
        let a = (self.s * RISTRETTO_BASEPOINT_POINT) + (self.c * public_value);
        let b = (self.s * p) + (self.c * q);
        let c_prime = ProofDLEQ::hash(&[&RISTRETTO_BASEPOINT_POINT, public_value, p, q, &a, &b]);
        c_prime == self.c
    }

    fn hash(elements: &[&RistrettoPoint]) -> Scalar {
        if elements.len() != 6 {
            panic!("Incorrect number of points sent: {}", elements.len());
        }
        let mut input = Vec::with_capacity(elements.len() * COMPRESSED_POINT_LEN);
        for ele in elements {
            input.extend(ele.compress().to_bytes());
        }
        let mut out = [0u8; 64];
        strobe_hash(&input, "ppoprf_dleq_hash", &mut out);
        Scalar::from_bytes_mod_order_wide(&out)
    }
}

// Server public key structure for PPOPRF, contains all elements of the
// form g^{sk_0},g^{t_i} for metadata tags t_i.
#[derive(Clone, Debug)]
pub struct ServerPublicKey {
    base_pk: RistrettoPoint,
    md_pks: HashMap<u8, RistrettoPoint>,
}
impl ServerPublicKey {
    fn get(&self, md: u8) -> Option<&RistrettoPoint> {
        self.md_pks.get(&md)
    }

    fn get_combined_pk_value(&self, md: u8) -> Result<RistrettoPoint, PPRFError> {
        let res = self.get(md);
        let md_pk = res.ok_or(PPRFError::BadTag { md })?;
        Ok(self.base_pk + md_pk)
    }
}

// The wrapper for PPOPRF evaluations (similar to standard OPRFs)
#[derive(Deserialize, Serialize)]
pub struct Evaluation {
    #[serde(deserialize_with = "ristretto_deserialize")]
    #[serde(serialize_with = "ristretto_serialize")]
    pub output: CompressedRistretto,
    pub proof: Option<ProofDLEQ>,
}

#[derive(Deserialize, Serialize)]
pub struct Point(
    #[serde(deserialize_with = "ristretto_deserialize")]
    #[serde(serialize_with = "ristretto_serialize")]
    pub CompressedRistretto,
);

fn ristretto_serialize<S>(o: &CompressedRistretto, s: S) -> Result<S::Ok, S::Error>
where
    S: ser::Serializer,
{
    s.serialize_str(&base64::encode(o.0))
}

fn ristretto_deserialize<'de, D>(d: D) -> Result<CompressedRistretto, D::Error>
where
    D: de::Deserializer<'de>,
{
    let s: &str = de::Deserialize::deserialize(d)?;
    let data = base64::decode(s).map_err(de::Error::custom)?;
    let fixed_data: [u8; 32] = data
        .try_into()
        .map_err(|_| de::Error::custom("Ristretto must be 32 bytes"))?;
    Ok(CompressedRistretto(fixed_data))
}

// The `Server` runs the server-side component of the PPOPRF protocol.
#[derive(Clone)]
pub struct Server {
    oprf_key: Scalar,
    public_key: ServerPublicKey,
    pprf: GGM,
}
impl Server {
    pub fn new(mds: Vec<u8>) -> Result<Self, PPRFError> {
        let mut csprng = OsRng;
        let oprf_key = Scalar::random(&mut csprng);
        let mut md_pks = HashMap::new();
        let pprf = GGM::setup();
        for &md in mds.iter() {
            let mut tag = [0u8; 32];
            pprf.eval(&[md], &mut tag)?;
            let ts = Scalar::from_bytes_mod_order(tag);
            md_pks.insert(md, ts * RISTRETTO_BASEPOINT_POINT);
        }
        Ok(Self {
            oprf_key,
            public_key: ServerPublicKey {
                base_pk: oprf_key * RISTRETTO_BASEPOINT_POINT,
                md_pks,
            },
            pprf,
        })
    }

    pub fn eval(&self, p: &Point, md: u8, verifiable: bool) -> Result<Evaluation, PPRFError> {
        let p = p.0;
        let point = p.decompress().unwrap();
        if self.public_key.get(md).is_none() {
            return Err(PPRFError::BadTag { md });
        }
        let mut tag = [0u8; 32];
        self.pprf.eval(&[md], &mut tag)?;
        let ts = Scalar::from_bytes_mod_order(tag);
        let tagged_key = self.oprf_key + ts;
        let exponent = tagged_key.invert();
        let eval_point = exponent * point;
        let mut proof = None;
        if verifiable {
            let public_value = self.public_key.get_combined_pk_value(md)?;
            proof = Some(ProofDLEQ::new(
                &tagged_key,
                &public_value,
                &eval_point,
                &point,
            ));
        }
        Ok(Evaluation {
            output: eval_point.compress(),
            proof,
        })
    }

    pub fn puncture(&mut self, md: u8) -> Result<(), PPRFError> {
        self.pprf.puncture(&[md])
    }

    pub fn get_public_key(&self) -> ServerPublicKey {
        self.public_key.clone()
    }
}

// The `Client` struct is essentially a collection of static functions
// for computing client-side operations in the PPOPRF protocol.
pub struct Client {}
impl Client {
    pub fn blind(input: &[u8]) -> (Point, Scalar) {
        let mut hashed_input = [0u8; 64];
        strobe_hash(input, "ppoprf_derive_client_input", &mut hashed_input);
        let point = RistrettoPoint::from_uniform_bytes(&hashed_input);
        let mut csprng = OsRng;
        let r = Scalar::random(&mut csprng);
        (Point((r * point).compress()), r)
    }

    pub fn verify(
        public_key: &ServerPublicKey,
        input: &RistrettoPoint,
        eval: &Evaluation,
        md: u8,
    ) -> bool {
        let Evaluation { output, proof } = eval;
        if let Ok(public_value) = public_key.get_combined_pk_value(md) {
            return proof.as_ref().unwrap().verify(
                &public_value,
                &output.decompress().unwrap(),
                input,
            );
        }
        false
    }

    pub fn unblind(p: &CompressedRistretto, r: &Scalar) -> CompressedRistretto {
        let point = p.decompress().unwrap();
        let r_inv = r.invert();
        (r_inv * point).compress()
    }

    pub fn finalize(input: &[u8], md: u8, unblinded: &CompressedRistretto, out: &mut [u8]) {
        if out.len() != 32 {
            panic!("Wrong output length!!: {:?}", out.len());
        }
        let point_bytes = unblinded.to_bytes();
        let mut hash_input = Vec::with_capacity(input.len() + 1 + point_bytes.len());
        hash_input.extend(input);
        hash_input.push(md);
        hash_input.extend(&point_bytes);
        let mut untruncated = vec![0u8; 64];
        strobe_hash(&hash_input, "ppoprf_finalize", &mut untruncated);
        out.copy_from_slice(&untruncated[..32]);
    }
}

fn strobe_hash(input: &[u8], label: &str, out: &mut [u8]) {
    if out.len() != DIGEST_LEN {
        panic!(
            "Output buffer length ({}) does not match intended output length ({})",
            out.len(),
            DIGEST_LEN
        );
    }
    let mut t = Strobe::new(label.as_bytes(), SecParam::B128);
    t.key(input, false);
    let mut rng: StrobeRng = t.into();
    rng.fill_bytes(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn end_to_end_eval_check_no_proof(
        server: &Server,
        c_input: &[u8],
        md: u8,
    ) -> (CompressedRistretto, CompressedRistretto) {
        let (blinded_point, r) = Client::blind(c_input);
        let evaluated = server.eval(&blinded_point, md, false).unwrap();
        let unblinded = Client::unblind(&evaluated.output, &r);

        let mut chk_inp = [0u8; 64];
        strobe_hash(c_input, "ppoprf_derive_client_input", &mut chk_inp);
        let p = Point(RistrettoPoint::from_uniform_bytes(&chk_inp).compress());
        let chk_eval = server.eval(&p, md, false).unwrap();
        (unblinded, chk_eval.output)
    }

    fn end_to_end_eval_check(
        server: &Server,
        c_input: &[u8],
        md: u8,
    ) -> (CompressedRistretto, CompressedRistretto) {
        let (blinded_point, r) = Client::blind(c_input);
        let evaluated = server.eval(&blinded_point, md, true).unwrap();
        if !Client::verify(
            &server.public_key,
            &blinded_point.0.decompress().unwrap(),
            &evaluated,
            md,
        ) {
            panic!("Failed to verify proof");
        }
        let unblinded = Client::unblind(&evaluated.output, &r);

        let mut chk_inp = [0u8; 64];
        strobe_hash(c_input, "ppoprf_derive_client_input", &mut chk_inp);
        let p = Point(RistrettoPoint::from_uniform_bytes(&chk_inp).compress());
        let chk_eval = server.eval(&p, md, false).unwrap();
        (unblinded, chk_eval.output)
    }

    fn end_to_end_no_verify(mds: &[u8], md: u8) {
        let server = Server::new(mds.to_vec()).unwrap();
        let input = b"some_test_input";
        let (unblinded, chk_eval) = end_to_end_eval_check_no_proof(&server, input, md);
        assert_eq!(chk_eval, unblinded);
        let mut eval_final = vec![0u8; 32];
        Client::finalize(input, md, &unblinded, &mut eval_final);
        let mut chk_final = vec![0u8; 32];
        Client::finalize(input, md, &chk_eval, &mut chk_final);
        assert_eq!(chk_final, eval_final);
    }

    fn end_to_end_verify(mds: &[u8], md: u8) {
        let server = Server::new(mds.to_vec()).unwrap();
        let input = b"some_test_input";
        let (unblinded, chk_eval) = end_to_end_eval_check(&server, input, md);
        assert_eq!(chk_eval, unblinded);
        let mut eval_final = vec![0u8; 32];
        Client::finalize(input, md, &unblinded, &mut eval_final);
        let mut chk_final = vec![0u8; 32];
        Client::finalize(input, md, &chk_eval, &mut chk_final);
        assert_eq!(chk_final, eval_final);
    }

    #[test]
    fn end_to_end_no_verify_single_tag() {
        end_to_end_no_verify(&[0u8], 0);
    }

    #[test]
    fn end_to_end_verify_single_tag() {
        end_to_end_verify(&[0u8], 0);
    }

    #[test]
    #[should_panic]
    fn bad_index() {
        end_to_end_verify(&[0u8], 1);
    }

    #[test]
    fn end_to_end_no_verify_multi_tag() {
        let mds = vec![0u8, 1, 2, 3, 4];
        end_to_end_no_verify(&mds, 0);
        end_to_end_no_verify(&mds, 1);
        end_to_end_no_verify(&mds, 2);
        end_to_end_no_verify(&mds, 3);
        end_to_end_no_verify(&mds, 4);
    }

    #[test]
    fn end_to_end_verify_multi_tag() {
        let mds = vec![0u8, 1, 2, 3, 4];
        end_to_end_verify(&mds, 0);
        end_to_end_verify(&mds, 1);
        end_to_end_verify(&mds, 2);
        end_to_end_verify(&mds, 3);
        end_to_end_verify(&mds, 4);
    }

    #[test]
    #[should_panic(expected = "NoPrefixFound")]
    fn end_to_end_puncture() {
        let mds = vec![0u8, 1];
        let mut server = Server::new(mds).unwrap();
        let (unblinded, chk_eval) = end_to_end_eval_check_no_proof(&server, b"some_test_input", 1);
        assert_eq!(chk_eval, unblinded);
        server.puncture(1).unwrap();
        let (unblinded1, chk_eval1) = end_to_end_eval_check_no_proof(&server, b"another_input", 0);
        assert_eq!(chk_eval1, unblinded1);
        end_to_end_eval_check_no_proof(&server, b"some_test_input", 1);
    }
}
