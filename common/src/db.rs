//! Connection options for the production MySQL (RDS).
//!
//! Every service reaches RDS over TLS, verified against Amazon's CA for
//! us-east-2 (`certs/rds-us-east-2-bundle.pem`, from
//! <https://truststore.pki.rds.amazonaws.com/us-east-2/us-east-2-bundle.pem>).
//!
//! Without TLS, MySQL 8's `caching_sha2_password` only lets a client in while
//! the server still holds the password in its cache. After a password change,
//! a reboot or a failover the cache is empty, the server asks for the full
//! exchange, and a client without TLS (and without RSA) is refused: "RSA auth
//! backend disabled". Over TLS the full exchange always works.

use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};
use std::str::FromStr;

/// Amazon RDS root certificates for us-east-2 (RSA2048, RSA4096, ECC384 G1).
const RDS_CA_BUNDLE: &[u8] = include_bytes!("../certs/rds-us-east-2-bundle.pem");

/// Parses a `mysql://` URL. An RDS host gets verified TLS: the certificate
/// must chain to Amazon's RDS CA and name the host, whatever `ssl-mode` the
/// URL names (the `granite/*` secrets say `DISABLED`). Other hosts (the local
/// test MySQL) keep the URL's own settings.
pub fn connect_options(url: &str) -> Result<MySqlConnectOptions, sqlx::Error> {
    let options = MySqlConnectOptions::from_str(url)?;
    Ok(if is_rds(options.get_host()) {
        options
            .ssl_mode(MySqlSslMode::VerifyIdentity)
            .ssl_ca_from_pem(RDS_CA_BUNDLE.to_vec())
    } else {
        options
    })
}

fn is_rds(host: &str) -> bool {
    host.to_ascii_lowercase().ends_with(".rds.amazonaws.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rds_hosts_get_verified_tls() {
        let options = connect_options(
            "mysql://u:p@database-1-copy.c9uq4w6eak98.us-east-2.rds.amazonaws.com:3306/main",
        )
        .unwrap();
        assert!(matches!(
            options.get_ssl_mode(),
            MySqlSslMode::VerifyIdentity
        ));
    }

    #[test]
    fn other_hosts_keep_the_url_settings() {
        let options = connect_options("mysql://root:granite@127.0.0.1:3307/granite_local").unwrap();
        assert!(matches!(options.get_ssl_mode(), MySqlSslMode::Preferred));
    }

    #[test]
    fn the_bundle_holds_the_three_us_east_2_roots() {
        let pem = std::str::from_utf8(RDS_CA_BUNDLE).unwrap();
        assert_eq!(pem.matches("BEGIN CERTIFICATE").count(), 3);
    }
}
