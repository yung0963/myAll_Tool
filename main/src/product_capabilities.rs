use one_core::storage::ConnectionType;

/// Product-level allowlist for the personal lightweight build.
///
/// Legacy variants stay readable in `one-core` so existing databases remain
/// compatible, but the application does not expose or open removed products.
pub(crate) const fn supports_connection_type(connection_type: ConnectionType) -> bool {
    !matches!(
        connection_type,
        ConnectionType::Redis | ConnectionType::MongoDB | ConnectionType::Rdp | ConnectionType::Vnc
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lightweight_build_excludes_removed_connection_products() {
        for connection_type in [
            ConnectionType::Redis,
            ConnectionType::MongoDB,
            ConnectionType::Rdp,
            ConnectionType::Vnc,
        ] {
            assert!(!supports_connection_type(connection_type));
        }

        assert!(supports_connection_type(ConnectionType::SshSftp));
        assert!(supports_connection_type(ConnectionType::Ftp));
        assert!(supports_connection_type(ConnectionType::Database));
    }
}
