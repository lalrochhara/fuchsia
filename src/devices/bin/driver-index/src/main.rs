// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{self, Context};
use fidl_fuchsia_driver_framework as fdf;
use fidl_fuchsia_driver_framework::{DriverIndexRequest, DriverIndexRequestStream};
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_zircon::{zx_status_t, Status};
use futures::prelude::*;
use serde::Deserialize;
use std::rc::Rc;

fn encode_err_to_stdio_err(error: bind::bytecode_common::BytecodeError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, format!("Error decoding bind file: {}", error))
}

#[derive(Deserialize)]
struct JsonDriver {
    bind_file: String,
    driver_url: String,
}

impl JsonDriver {
    fn to_driver(self) -> std::io::Result<Driver> {
        let bytes = std::fs::read(&self.bind_file)?;
        Driver::create(self.driver_url, bytes).map_err(encode_err_to_stdio_err)
    }
}

/// Wraps all hosted protocols into a single type that can be matched against
/// and dispatched.
enum IncomingRequest {
    DriverIndexProtocol(DriverIndexRequestStream),
}

struct Driver {
    bind_rules: bind::decode_bind_rules::DecodedBindRules,
    url: String,
}

impl Driver {
    fn matches(
        &self,
        properties: bind::match_bind::DeviceProperties,
    ) -> Result<bool, bind::bytecode_common::BytecodeError> {
        // TODO(fxbug.dev/77377): This needs to be updated when DeviceMatcher no longer consumes
        // the bind program.
        bind::match_bind::DeviceMatcher::new(self.bind_rules.clone(), properties).match_bind()
    }

    fn create(
        url: String,
        bind_rules: Vec<u8>,
    ) -> Result<Driver, bind::bytecode_common::BytecodeError> {
        Ok(Driver {
            bind_rules: bind::decode_bind_rules::DecodedBindRules::new(bind_rules)?,
            url: url,
        })
    }
}

impl std::fmt::Display for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.url)
    }
}

struct Indexer {
    drivers: Vec<Driver>,
}

impl Indexer {
    fn new(drivers: Vec<JsonDriver>) -> Result<Indexer, anyhow::Error> {
        let drivers: Result<Vec<_>, _> = drivers.into_iter().map(|d| d.to_driver()).collect();
        Ok(Indexer { drivers: drivers? })
    }

    #[allow(dead_code)]
    fn add_driver(&mut self, driver: Driver) {
        self.drivers.push(driver);
    }

    fn match_driver(&self, args: fdf::NodeAddArgs) -> fdf::DriverIndexMatchDriverResult {
        if args.properties.is_none() {
            return Err(Status::INVALID_ARGS.into_raw());
        }
        let properties = args.properties.unwrap();
        let properties = node_to_device_property(&properties)?;
        for driver in &self.drivers {
            match driver.matches(properties.clone()) {
                Ok(true) => {
                    return Ok(fdf::MatchedDriver {
                        url: Some(driver.url.clone()),
                        ..fdf::MatchedDriver::EMPTY
                    });
                }
                Ok(false) => continue,
                Err(e) => {
                    // If a driver has a bind error we will keep trying to match the other drivers
                    // instead of returning an error.
                    eprintln!("Driver {}: bind error: {}", driver, e);
                    continue;
                }
            }
        }
        Err(Status::NOT_FOUND.into_raw())
    }
}

fn node_to_device_property(
    node_properties: &Vec<fdf::NodeProperty>,
) -> Result<bind::match_bind::DeviceProperties, zx_status_t> {
    let mut device_properties = bind::match_bind::DeviceProperties::new();

    for property in node_properties {
        if property.key.is_none() || property.value.is_none() {
            return Err(Status::INVALID_ARGS.into_raw());
        }
        device_properties.insert(
            bind::match_bind::PropertyKey::NumberKey(property.key.unwrap().into()),
            bind::compiler::Symbol::NumberValue(property.value.unwrap().into()),
        );
    }
    Ok(device_properties)
}

async fn run_index_server(
    indexer: Rc<Indexer>,
    stream: DriverIndexRequestStream,
) -> Result<(), anyhow::Error> {
    stream
        .map(|result| result.context("failed request"))
        .try_for_each(|request| async {
            let indexer = indexer.clone();
            match request {
                DriverIndexRequest::MatchDriver { args, responder } => {
                    responder
                        .send(&mut indexer.match_driver(args))
                        .context("error sending response")?;
                }
            }
            Ok(())
        })
        .await?;
    Ok(())
}

#[fasync::run_singlethreaded]
async fn main() -> Result<(), anyhow::Error> {
    let mut service_fs = ServiceFs::new_local();

    let data = std::fs::read_to_string("/pkg/config/driver-index.json")?;
    let drivers: Vec<JsonDriver> = serde_json::from_str(&data)?;
    let index = Rc::new(Indexer::new(drivers)?);

    service_fs.dir("svc").add_fidl_service(IncomingRequest::DriverIndexProtocol);

    service_fs.take_and_serve_directory_handle().context("failed to serve outgoing namespace")?;

    service_fs
        .for_each_concurrent(None, |request: IncomingRequest| async {
            // match on `request` and handle each protocol.
            match request {
                IncomingRequest::DriverIndexProtocol(stream) => {
                    run_index_server(index.clone(), stream).await
                }
            }
            .unwrap_or_else(|e| println!("{:?}", e))
        })
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fasync::run_singlethreaded(test)]
    async fn match_driver_empty_indexer() {
        let index = Rc::new(Indexer { drivers: vec![] });

        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<fdf::DriverIndexMarker>().unwrap();

        let (a, _) = future::join(run_index_server(index.clone(), stream), async move {
            let property = fdf::NodeProperty {
                key: Some(0x10),
                value: Some(0x20),
                ..fdf::NodeProperty::EMPTY
            };
            let args =
                fdf::NodeAddArgs { properties: Some(vec![property]), ..fdf::NodeAddArgs::EMPTY };
            let result = proxy.match_driver(args).await.unwrap();
            assert_eq!(result, Err(Status::NOT_FOUND.into_raw()));
        })
        .await;
        a.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn match_driver_no_node_properties() {
        let bind_rules = bind::compiler::BindProgram {
            symbol_table: bind::compiler::SymbolTable::new(),
            instructions: vec![],
            use_new_bytecode: true,
        };
        let byte_code = bind_rules.encode_to_bytecode().unwrap();
        let bind_rules = bind::decode_bind_rules::DecodedBindRules::new(byte_code).unwrap();
        let index = Rc::new(Indexer {
            drivers: vec![Driver { bind_rules: bind_rules, url: "my-url.cmx".to_string() }],
        });

        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<fdf::DriverIndexMarker>().unwrap();

        let (a, _) = future::join(run_index_server(index.clone(), stream), async move {
            let args = fdf::NodeAddArgs::EMPTY;
            let result = proxy.match_driver(args).await.unwrap();
            assert_eq!(result, Err(Status::INVALID_ARGS.into_raw()));
        })
        .await;
        a.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn match_driver_bind_error() {
        let bind_rules = bind::compiler::BindProgram {
            symbol_table: bind::compiler::SymbolTable::new(),
            instructions: vec![bind::compiler::SymbolicInstructionInfo {
                location: None,
                instruction: bind::compiler::SymbolicInstruction::AbortIfNotEqual {
                    lhs: bind::compiler::Symbol::DeprecatedKey(10),
                    rhs: bind::compiler::Symbol::NumberValue(1),
                },
            }],
            use_new_bytecode: true,
        };
        let byte_code = bind_rules.encode_to_bytecode().unwrap();
        let bind_rules = bind::decode_bind_rules::DecodedBindRules::new(byte_code).unwrap();
        let index = Rc::new(Indexer {
            drivers: vec![Driver { bind_rules: bind_rules, url: "my-url.cmx".to_string() }],
        });

        // This property does not match the above program.
        let property =
            fdf::NodeProperty { key: Some(10), value: Some(20), ..fdf::NodeProperty::EMPTY };

        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<fdf::DriverIndexMarker>().unwrap();
        let (a, _) = future::join(run_index_server(index.clone(), stream), async move {
            let args =
                fdf::NodeAddArgs { properties: Some(vec![property]), ..fdf::NodeAddArgs::EMPTY };
            let result = proxy.match_driver(args).await.unwrap();
            assert_eq!(result, Err(Status::NOT_FOUND.into_raw()));
        })
        .await;
        a.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn match_driver_success() {
        let bind_rules = bind::compiler::BindProgram {
            symbol_table: bind::compiler::SymbolTable::new(),
            instructions: vec![bind::compiler::SymbolicInstructionInfo {
                location: None,
                instruction: bind::compiler::SymbolicInstruction::AbortIfNotEqual {
                    lhs: bind::compiler::Symbol::DeprecatedKey(10),
                    rhs: bind::compiler::Symbol::NumberValue(1),
                },
            }],
            use_new_bytecode: true,
        };
        let byte_code = bind_rules.encode_to_bytecode().unwrap();
        let bind_rules = bind::decode_bind_rules::DecodedBindRules::new(byte_code).unwrap();

        let url = "my-url.cmx".to_string();
        let index =
            Rc::new(Indexer { drivers: vec![Driver { bind_rules: bind_rules, url: url.clone() }] });

        // This property does match the above program
        let property =
            fdf::NodeProperty { key: Some(10), value: Some(1), ..fdf::NodeProperty::EMPTY };

        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<fdf::DriverIndexMarker>().unwrap();
        let (a, _) = future::join(run_index_server(index.clone(), stream), async move {
            let args =
                fdf::NodeAddArgs { properties: Some(vec![property]), ..fdf::NodeAddArgs::EMPTY };
            let result = proxy.match_driver(args).await.unwrap();
            let received_url = result.unwrap().url.unwrap();
            assert_eq!(url.to_string(), received_url);
        })
        .await;
        a.unwrap();
    }

    // This test depends on '/pkg/config/drivers_for_test.json' existing in the test package.
    // The test reads that json file to determine which bind programs to read and index.
    #[fasync::run_singlethreaded(test)]
    async fn read_from_json() {
        let mut index = Indexer { drivers: vec![] };

        let data = std::fs::read_to_string("/pkg/config/drivers_for_test.json").unwrap();
        let drivers: Vec<JsonDriver> = serde_json::from_str(&data).unwrap();
        for driver in drivers {
            index.add_driver(driver.to_driver().unwrap());
        }

        let index = Rc::new(index);
        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<fdf::DriverIndexMarker>().unwrap();

        let (server_result, _) =
            future::join(run_index_server(index.clone(), stream), async move {
                // Check the value from the 'test-bind' binary. This should match my-driver.cm
                let property = fdf::NodeProperty {
                    key: Some(bind::ddk_bind_constants::BIND_PROTOCOL),
                    value: Some(1),
                    ..fdf::NodeProperty::EMPTY
                };
                let args = fdf::NodeAddArgs {
                    properties: Some(vec![property]),
                    ..fdf::NodeAddArgs::EMPTY
                };
                let result = proxy.match_driver(args).await.unwrap();
                let received_url = result.unwrap().url.unwrap();
                assert_eq!(
                    "fuchsia-pkg://my-test-driver#meta/my-driver.cm".to_string(),
                    received_url
                );

                // Check the value from the 'test-bind2' binary. This should match my-driver2.cm
                let property = fdf::NodeProperty {
                    key: Some(bind::ddk_bind_constants::BIND_PROTOCOL),
                    value: Some(2),
                    ..fdf::NodeProperty::EMPTY
                };
                let args = fdf::NodeAddArgs {
                    properties: Some(vec![property]),
                    ..fdf::NodeAddArgs::EMPTY
                };
                let result = proxy.match_driver(args).await.unwrap();
                let received_url = result.unwrap().url.unwrap();
                assert_eq!(
                    "fuchsia-pkg://my-test-driver#meta/my-driver2.cm".to_string(),
                    received_url
                );

                // Check an unknown value. This should return the NOT_FOUND error.
                let property = fdf::NodeProperty {
                    key: Some(bind::ddk_bind_constants::BIND_PROTOCOL),
                    value: Some(3),
                    ..fdf::NodeProperty::EMPTY
                };
                let args = fdf::NodeAddArgs {
                    properties: Some(vec![property]),
                    ..fdf::NodeAddArgs::EMPTY
                };
                let result = proxy.match_driver(args).await.unwrap();
                assert_eq!(result, Err(Status::NOT_FOUND.into_raw()));
            })
            .await;
        server_result.unwrap();
    }
}
