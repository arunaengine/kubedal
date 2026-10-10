//! Helpers that turn library errors into CSI gRPC status codes.

use tonic::Status;

/// Map a Kubernetes API error to a gRPC status.
///
/// `what` describes the object that was requested, for example `DataNode default/data`.
pub fn kube_status(err: kube::Error, what: &str) -> Status {
    match &err {
        kube::Error::Api(status) if status.code == 404 => {
            Status::not_found(format!("{what} not found"))
        }
        kube::Error::Api(status) if status.code == 403 => {
            Status::permission_denied(format!("access to {what} denied: {}", status.message))
        }
        kube::Error::Api(status) if status.code == 400 || status.code == 422 => {
            Status::invalid_argument(format!("{what}: {}", status.message))
        }
        _ => Status::unavailable(format!("failed to get {what}: {err}")),
    }
}

/// Map an OpenDAL error to a gRPC status.
///
/// `context` describes what kubedal tried to do, for example `list data/`.
pub fn opendal_status(err: opendal::Error, context: &str) -> Status {
    use opendal::ErrorKind;
    let message = format!("{context}: {err}");
    match err.kind() {
        ErrorKind::NotFound => Status::not_found(message),
        ErrorKind::PermissionDenied => Status::permission_denied(message),
        ErrorKind::ConfigInvalid | ErrorKind::Unsupported => Status::invalid_argument(message),
        ErrorKind::RateLimited => Status::unavailable(message),
        _ if err.is_temporary() => Status::unavailable(message),
        _ => Status::internal(message),
    }
}

/// Map a local IO error to a gRPC status.
pub fn io_status(err: std::io::Error, context: &str) -> Status {
    let message = format!("{context}: {err}");
    match err.kind() {
        std::io::ErrorKind::NotFound => Status::not_found(message),
        std::io::ErrorKind::PermissionDenied => Status::permission_denied(message),
        std::io::ErrorKind::StorageFull => Status::resource_exhausted(message),
        _ => Status::internal(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;

    #[test]
    fn opendal_not_found_maps_to_not_found() {
        let err = opendal::Error::new(opendal::ErrorKind::NotFound, "missing");
        assert_eq!(opendal_status(err, "stat a").code(), Code::NotFound);
    }

    #[test]
    fn opendal_unexpected_maps_to_internal() {
        let err = opendal::Error::new(opendal::ErrorKind::Unexpected, "boom");
        assert_eq!(opendal_status(err, "read a").code(), Code::Internal);
    }

    #[test]
    fn opendal_temporary_maps_to_unavailable() {
        let err = opendal::Error::new(opendal::ErrorKind::Unexpected, "boom").set_temporary();
        assert_eq!(opendal_status(err, "read a").code(), Code::Unavailable);
    }

    #[test]
    fn io_errors_keep_their_meaning() {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(io_status(err, "open").code(), Code::NotFound);
        let err = std::io::Error::from(std::io::ErrorKind::StorageFull);
        assert_eq!(io_status(err, "write").code(), Code::ResourceExhausted);
    }
}
