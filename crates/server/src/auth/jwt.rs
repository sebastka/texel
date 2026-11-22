//! ID tokens: their signature checked with the provider's keys (JWKS), with
//! `ring` (the TLS provider already), and their claims. Asymmetric
//! algorithms only: never `none` or HMAC.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey};
use serde::Deserialize;
use serde_json::{Map, Value};

/// Clock skew tolerated, in seconds.
const LEEWAY: u64 = 60;

#[derive(Clone, Debug, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Jwk {
    pub kty: String,
    #[serde(default)]
    pub kid: Option<String>,
    #[serde(default, rename = "use")]
    pub use_: Option<String>,
    #[serde(default)]
    pub n: Option<String>,
    #[serde(default)]
    pub e: Option<String>,
    #[serde(default)]
    pub crv: Option<String>,
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub y: Option<String>,
}

/// What an ID token must say.
pub struct Expected<'a> {
    pub issuer: &'a str,
    pub client_id: &'a str,
    pub nonce: &'a str,
    /// Unix seconds.
    pub now: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum JwtError {
    Malformed,
    /// No key of the JWKS has this id (the keys may have rotated).
    UnknownKey,
    Algorithm,
    Signature,
    Claims(&'static str),
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("a malformed ID token"),
            Self::UnknownKey => f.write_str("an ID token signed with an unknown key"),
            Self::Algorithm => f.write_str("an ID token with an unsupported algorithm"),
            Self::Signature => f.write_str("an ID token with a bad signature"),
            Self::Claims(what) => write!(f, "an ID token with a bad claim: {what}"),
        }
    }
}

fn b64(s: &str) -> Result<Vec<u8>, JwtError> {
    URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .map_err(|_| JwtError::Malformed)
}

/// The token's key id, to refresh the JWKS when unknown.
pub fn key_id(token: &str) -> Option<String> {
    let header = token.split('.').next()?;
    let header: Value = serde_json::from_slice(&b64(header).ok()?).ok()?;
    header.get("kid")?.as_str().map(str::to_owned)
}

/// Checks the token's signature and claims; returns its claims.
pub fn verify(token: &str, jwks: &Jwks, expected: &Expected) -> Result<Map<String, Value>, JwtError> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(JwtError::Malformed);
    };
    let header: Value = serde_json::from_slice(&b64(h)?).map_err(|_| JwtError::Malformed)?;
    let alg = header.get("alg").and_then(Value::as_str).ok_or(JwtError::Malformed)?;
    let kid = header.get("kid").and_then(Value::as_str);
    let signed = &token[..h.len() + 1 + p.len()];
    let sig = b64(s)?;
    let candidates: Vec<&Jwk> = jwks
        .keys
        .iter()
        .filter(|k| k.use_.as_deref().is_none_or(|u| u == "sig"))
        .filter(|k| kid.is_none_or(|id| k.kid.as_deref() == Some(id)))
        .collect();
    if candidates.is_empty() {
        return Err(JwtError::UnknownKey);
    }
    if !candidates
        .iter()
        .any(|k| check(alg, k, signed.as_bytes(), &sig).unwrap_or(false))
    {
        // An algorithm we do not take is not a bad signature.
        return Err(match check(alg, candidates[0], signed.as_bytes(), &sig) {
            Err(e) => e,
            Ok(_) => JwtError::Signature,
        });
    }
    let claims: Map<String, Value> = serde_json::from_slice(&b64(p)?).map_err(|_| JwtError::Malformed)?;
    check_claims(&claims, expected)?;
    Ok(claims)
}

/// Whether `sig` signs `msg` with key `k` under `alg`.
fn check(alg: &str, k: &Jwk, msg: &[u8], sig: &[u8]) -> Result<bool, JwtError> {
    let rsa = |params: &'static signature::RsaParameters| -> Result<bool, JwtError> {
        if k.kty != "RSA" {
            return Ok(false);
        }
        let (Some(n), Some(e)) = (&k.n, &k.e) else {
            return Ok(false);
        };
        let (n, e) = (b64(n)?, b64(e)?);
        Ok(RsaPublicKeyComponents { n: &n, e: &e }.verify(params, msg, sig).is_ok())
    };
    let ec = |curve: &str, a: &'static signature::EcdsaVerificationAlgorithm| -> Result<bool, JwtError> {
        if k.kty != "EC" || k.crv.as_deref() != Some(curve) {
            return Ok(false);
        }
        let (Some(x), Some(y)) = (&k.x, &k.y) else {
            return Ok(false);
        };
        let mut point = vec![4];
        point.extend(b64(x)?);
        point.extend(b64(y)?);
        Ok(UnparsedPublicKey::new(a, point).verify(msg, sig).is_ok())
    };
    match alg {
        "RS256" => rsa(&signature::RSA_PKCS1_2048_8192_SHA256),
        "RS384" => rsa(&signature::RSA_PKCS1_2048_8192_SHA384),
        "RS512" => rsa(&signature::RSA_PKCS1_2048_8192_SHA512),
        "PS256" => rsa(&signature::RSA_PSS_2048_8192_SHA256),
        "PS384" => rsa(&signature::RSA_PSS_2048_8192_SHA384),
        "PS512" => rsa(&signature::RSA_PSS_2048_8192_SHA512),
        "ES256" => ec("P-256", &signature::ECDSA_P256_SHA256_FIXED),
        "ES384" => ec("P-384", &signature::ECDSA_P384_SHA384_FIXED),
        "EdDSA" => {
            if k.kty != "OKP" || k.crv.as_deref() != Some("Ed25519") {
                return Ok(false);
            }
            let Some(x) = &k.x else { return Ok(false) };
            Ok(UnparsedPublicKey::new(&signature::ED25519, b64(x)?)
                .verify(msg, sig)
                .is_ok())
        }
        _ => Err(JwtError::Algorithm),
    }
}

fn check_claims(c: &Map<String, Value>, ex: &Expected) -> Result<(), JwtError> {
    let text = |k: &str| c.get(k).and_then(Value::as_str);
    let number = |k: &str| c.get(k).and_then(Value::as_u64);
    if text("iss") != Some(ex.issuer) {
        return Err(JwtError::Claims("issuer"));
    }
    let audiences: Vec<&str> = match c.get("aud") {
        Some(Value::String(a)) => vec![a],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !audiences.contains(&ex.client_id) {
        return Err(JwtError::Claims("audience"));
    }
    // With other audiences, the authorized party must be us.
    let azp = text("azp");
    if azp.is_some_and(|a| a != ex.client_id) || (audiences.len() > 1 && azp.is_none()) {
        return Err(JwtError::Claims("authorized party"));
    }
    match number("exp") {
        Some(exp) if exp + LEEWAY > ex.now => {}
        _ => return Err(JwtError::Claims("expiry")),
    }
    if number("iat").is_some_and(|iat| iat > ex.now + LEEWAY) {
        return Err(JwtError::Claims("issue time"));
    }
    if text("nonce") != Some(ex.nonce) {
        return Err(JwtError::Claims("nonce"));
    }
    if text("sub").is_none_or(str::is_empty) {
        return Err(JwtError::Claims("subject"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair};
    use serde_json::json;

    use super::*;

    const ISSUER: &str = "https://sso.example.com";
    const CLIENT: &str = "texel";

    fn enc(v: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(v)
    }

    fn token(alg: &str, kid: &str, claims: &Value, sign: impl Fn(&[u8]) -> Vec<u8>) -> String {
        let h = enc(json!({ "alg": alg, "kid": kid, "typ": "JWT" }).to_string().as_bytes());
        let p = enc(claims.to_string().as_bytes());
        let signed = format!("{h}.{p}");
        let sig = sign(signed.as_bytes());
        format!("{signed}.{}", enc(&sig))
    }

    fn claims() -> Value {
        json!({ "iss": ISSUER, "aud": CLIENT, "sub": "u1", "exp": 2_000, "iat": 1_000, "nonce": "n" })
    }

    fn expected() -> Expected<'static> {
        Expected {
            issuer: ISSUER,
            client_id: CLIENT,
            nonce: "n",
            now: 1_500,
        }
    }

    fn es256() -> (EcdsaKeyPair, Jwks) {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let point = pair.public_key().as_ref();
        let jwks = Jwks {
            keys: vec![Jwk {
                kty: "EC".into(),
                kid: Some("k1".into()),
                use_: Some("sig".into()),
                n: None,
                e: None,
                crv: Some("P-256".into()),
                x: Some(enc(&point[1..33])),
                y: Some(enc(&point[33..65])),
            }],
        };
        (pair, jwks)
    }

    fn sign_es256(pair: &EcdsaKeyPair) -> impl Fn(&[u8]) -> Vec<u8> + '_ {
        move |m| pair.sign(&SystemRandom::new(), m).unwrap().as_ref().to_vec()
    }

    #[test]
    fn a_good_es256_token() {
        let (pair, jwks) = es256();
        let t = token("ES256", "k1", &claims(), sign_es256(&pair));
        let c = verify(&t, &jwks, &expected()).unwrap();
        assert_eq!(c["sub"], "u1");
        assert_eq!(key_id(&t).as_deref(), Some("k1"));
    }

    #[test]
    fn a_good_eddsa_token() {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let jwks = Jwks {
            keys: vec![Jwk {
                kty: "OKP".into(),
                kid: Some("ed".into()),
                use_: None,
                n: None,
                e: None,
                crv: Some("Ed25519".into()),
                x: Some(enc(pair.public_key().as_ref())),
                y: None,
            }],
        };
        let t = token("EdDSA", "ed", &claims(), |m| pair.sign(m).as_ref().to_vec());
        assert!(verify(&t, &jwks, &expected()).is_ok());
    }

    #[test]
    fn refuses_bad_tokens() {
        let (pair, jwks) = es256();
        let sign = sign_es256(&pair);
        let with = |f: &dyn Fn(&mut Value)| {
            let mut c = claims();
            f(&mut c);
            token("ES256", "k1", &c, &sign)
        };
        let ex = expected();
        assert_eq!(
            verify(&with(&|c| c["iss"] = json!("https://evil")), &jwks, &ex),
            Err(JwtError::Claims("issuer"))
        );
        assert_eq!(
            verify(&with(&|c| c["aud"] = json!("other")), &jwks, &ex),
            Err(JwtError::Claims("audience"))
        );
        assert_eq!(
            verify(&with(&|c| c["aud"] = json!([CLIENT, "other"])), &jwks, &ex),
            Err(JwtError::Claims("authorized party"))
        );
        assert!(
            verify(
                &with(&|c| {
                    c["aud"] = json!([CLIENT, "other"]);
                    c["azp"] = json!(CLIENT)
                }),
                &jwks,
                &ex
            )
            .is_ok()
        );
        assert_eq!(
            verify(&with(&|c| c["exp"] = json!(1_000)), &jwks, &ex),
            Err(JwtError::Claims("expiry"))
        );
        assert_eq!(
            verify(&with(&|c| c["iat"] = json!(9_000)), &jwks, &ex),
            Err(JwtError::Claims("issue time"))
        );
        assert_eq!(
            verify(&with(&|c| c["nonce"] = json!("x")), &jwks, &ex),
            Err(JwtError::Claims("nonce"))
        );
        // A changed payload: the signature no longer matches.
        let good = token("ES256", "k1", &claims(), &sign);
        let parts: Vec<&str> = good.split('.').collect();
        let forged = format!(
            "{}.{}.{}",
            parts[0],
            enc(json!({"sub": "admin"}).to_string().as_bytes()),
            parts[2]
        );
        assert_eq!(verify(&forged, &jwks, &ex), Err(JwtError::Signature));
        // Unsigned, HMAC, an unknown key.
        assert_eq!(
            verify(&token("none", "k1", &claims(), |_| Vec::new()), &jwks, &ex),
            Err(JwtError::Algorithm)
        );
        assert_eq!(
            verify(&token("HS256", "k1", &claims(), |_| vec![1; 32]), &jwks, &ex),
            Err(JwtError::Algorithm)
        );
        assert_eq!(
            verify(&token("ES256", "k9", &claims(), &sign), &jwks, &ex),
            Err(JwtError::UnknownKey)
        );
    }
}
