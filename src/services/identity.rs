use tonic::{Request, Response, Status};
use crate::csi::{
    identity_server::Identity, GetPluginCapabilitiesRequest, GetPluginCapabilitiesResponse, GetPluginInfoRequest, GetPluginInfoResponse, PluginCapability, ProbeRequest, ProbeResponse
};

#[derive(Debug, Default)]
pub struct IdentityService {
    name: String,
    version: String,
}

impl IdentityService {
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
        }
    }
}

#[tonic::async_trait]
impl Identity for IdentityService {
    async fn get_plugin_info(
        &self,
        _request: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        let response = GetPluginInfoResponse {
            name: self.name.clone(),
            vendor_version: self.version.clone(),
            manifest: std::collections::HashMap::new(),
        };

        Ok(Response::new(response))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: Request<GetPluginCapabilitiesRequest>,
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        // For our dummy driver, advertise controller service capability
        let controller_service = PluginCapability {
            r#type: Some(
                crate::csi::plugin_capability::Type::Service(
                    crate::csi::plugin_capability::Service {
                        r#type: crate::csi::plugin_capability::service::Type::ControllerService.into(),
                    }
                )
            ),
        };

        let volume_expansion = PluginCapability {
            r#type: Some(
                crate::csi::plugin_capability::Type::VolumeExpansion(
                    crate::csi::plugin_capability::VolumeExpansion {
                        r#type: crate::csi::plugin_capability::volume_expansion::Type::Online.into(),
                    }
                )
            ),
        };

        let response = GetPluginCapabilitiesResponse {
            capabilities: vec![controller_service, volume_expansion],
        };

        Ok(Response::new(response))
    }

    async fn probe(
        &self,
        _request: Request<ProbeRequest>,
    ) -> Result<Response<ProbeResponse>, Status> {
        // Our dummy driver is always healthy
        let response = ProbeResponse {
            ready: Some(true),
        };

        Ok(Response::new(response))
    }
}