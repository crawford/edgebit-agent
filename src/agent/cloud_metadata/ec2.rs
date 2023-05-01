use std::collections::HashMap;

use aws_config::imds::client::Client;
use anyhow::Result;
use serde::Deserialize;
use log::*;

use crate::label::*;

#[derive(Deserialize)]
struct InstanceIdentityDocument {
    #[serde(rename = "accountId")]
    account_id: String,

    //architecture: String,

    #[serde(rename = "availabilityZone")]
    availability_zone: String,

    #[serde(rename = "imageId")]
    image_id: String,

    #[serde(rename = "instanceId")]
    instance_id: String,

    //#[serde(rename = "instanceType")]
    //instance_type: String,

    //#[serde(rename = "privateIp")]
    //privateIp: Option<String>,

    region: String,
}

impl InstanceIdentityDocument {
    fn from_str(doc_str: &str) -> Result<Self> {
        Ok(serde_json::from_str(doc_str)?)
    }

    async fn from_imds() -> Result<Self> {
        let client = Client::builder()
            .build()
            .await?;

        let doc = client.get("/2022-09-24/dynamic/instance-identity/document").await?;

        debug!("Loaded IdentityDocument: {doc}");

        Self::from_str(&doc)
    }
}

pub struct Ec2Metadata {
    doc: InstanceIdentityDocument,
}

impl Ec2Metadata {
    pub async fn load() -> Result<Self> {
        let doc = InstanceIdentityDocument::from_imds().await?;

        Ok(Ec2Metadata{
            doc,
        })
    }
}

impl super::MetadataProvider for Ec2Metadata {
    fn host_labels(&self) -> HashMap<String, String> {
        [
            (LABEL_CLOUD.to_string(), "ec2".to_string()),
            (LABEL_INSTANCE_ID.to_string(), self.doc.instance_id.clone()),
            (LABEL_IMAGE_ID.to_string(), self.doc.image_id.clone()),
            (LABEL_REGION.to_string(), self.doc.region.clone()),
            (LABEL_ZONE.to_string(), self.doc.availability_zone.clone()),
            (LABEL_ACCOUNT_ID.to_string(), self.doc.account_id.clone()),
        ].into()
    }

    fn container_labels(&self, _id: &str) -> HashMap<String, String> {
        self.host_labels()
    }
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr};

    use assert2::assert;
    use hyper::{Server, Request, Response, Body, StatusCode};
    use hyper::service::{make_service_fn, service_fn};

    use super::*;

    const TEST_METADATA: &str = r#"
{
  "accountId" : "601263177651",
  "architecture" : "x86_64",
  "availabilityZone" : "us-east-1d",
  "billingProducts" : null,
  "devpayProductCodes" : null,
  "marketplaceProductCodes" : null,
  "imageId" : "ami-0557a15b87f6559cf",
  "instanceId" : "i-01d1e9aa7a573262f",
  "instanceType" : "t2.medium",
  "kernelId" : null,
  "pendingTime" : "2023-04-18T23:02:22Z",
  "privateIp" : "172.31.81.118",
  "ramdiskId" : null,
  "region" : "us-east-1",
  "version" : "2017-09-30"
}"#;

    async fn mock_metadata_svc(req: Request<Body>) -> std::result::Result<Response<Body>, hyper::Error> {
        let resp = match req.uri() {
            uri if uri == "/latest/api/token" => {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
                    .body(Body::from("AQAAAID1mYTPepz28ILQ88CZW6r62fL9ur4jSIKniBoIm2YkofZ9Dw=="))
                    .unwrap()
            },
            _ => {
                assert!(req.uri() == "/2022-09-24/dynamic/instance-identity/document");

                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from(TEST_METADATA))
                    .unwrap()
            }
        };

        Ok(resp)
    }

    #[tokio::test]
    async fn test_ec2() {
        use super::super::MetadataProvider;

        let addr = SocketAddr::V4("127.0.0.1:9991".parse().unwrap());

        let make_svc = make_service_fn(|_| async {
            Ok::<_, hyper::Error>(service_fn(mock_metadata_svc))
        });
        let server = Server::bind(&addr).serve(make_svc);
        let server_task = tokio::task::spawn(server);

        std::env::set_var("AWS_EC2_METADATA_SERVICE_ENDPOINT", "http://localhost:9991");

        let metadata = Ec2Metadata::load().await.unwrap();
        let labels = metadata.host_labels();

        assert!(labels.get(LABEL_CLOUD).unwrap() == "ec2");
        assert!(labels.get(LABEL_INSTANCE_ID).unwrap() == "i-01d1e9aa7a573262f");
        assert!(labels.get(LABEL_IMAGE_ID).unwrap() == "ami-0557a15b87f6559cf");
        assert!(labels.get(LABEL_REGION).unwrap() == "us-east-1");
        assert!(labels.get(LABEL_ZONE).unwrap() == "us-east-1d");
        assert!(labels.get(LABEL_ACCOUNT_ID).unwrap() == "601263177651");

        server_task.abort();
        _ = server_task.await;
    }
}
