// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::agent::Payload;
use crate::agent::{AgentError, Context, Invocation, InvocationResult, Lifespan};
use crate::base::SettingType;
use crate::blueprint_definition;
use crate::event::{restore, Event, Publisher};
use crate::handler::base::{Error, Payload as HandlerPayload, Request};
use crate::handler::device_storage::DeviceStorageAccess;
use crate::message::base::Audience;
use crate::policy::PolicyType;
use crate::service;
use fuchsia_async as fasync;
use fuchsia_syslog::{fx_log_err, fx_log_info};
use std::collections::HashSet;

blueprint_definition!("restore_agent", crate::agent::restore_agent::RestoreAgent::create);

/// The Restore Agent is responsible for signaling to all components to restore
/// external sources to the last known value. It is invoked during startup.
#[derive(Debug)]
pub struct RestoreAgent {
    messenger: service::message::Messenger,
    event_publisher: Publisher,
    available_components: HashSet<SettingType>,
    available_policies: HashSet<PolicyType>,
}

impl DeviceStorageAccess for RestoreAgent {
    const STORAGE_KEYS: &'static [&'static str] = &[];
}

impl RestoreAgent {
    async fn create(context: Context) {
        let mut agent = RestoreAgent {
            messenger: context.create_messenger().await.expect("should acquire messenger"),
            event_publisher: context.get_publisher(),
            available_components: context.available_components,
            available_policies: context.available_policies,
        };

        let mut receptor = context.receptor;
        fasync::Task::spawn(async move {
            while let Ok((Payload::Invocation(invocation), client)) =
                receptor.next_of::<Payload>().await
            {
                client.reply(Payload::Complete(agent.handle(invocation).await).into()).send().ack();
            }
        })
        .detach();
    }

    async fn handle(&mut self, invocation: Invocation) -> InvocationResult {
        match invocation.lifespan {
            Lifespan::Initialization => {
                for component in self.available_components.clone() {
                    let mut receptor = self
                        .messenger
                        .message(
                            HandlerPayload::Request(Request::Restore).into(),
                            Audience::Address(service::Address::Handler(component)),
                        )
                        .send();

                    if let HandlerPayload::Response(response) = receptor
                        .next_of::<HandlerPayload>()
                        .await
                        .map_err(|_| AgentError::UnexpectedError)?
                        .0
                    {
                        match response {
                            Ok(_) => {
                                continue;
                            }
                            Err(Error::UnimplementedRequest(setting_type, _)) => {
                                self.event_publisher
                                    .send_event(Event::Restore(restore::Event::NoOp(setting_type)));
                                continue;
                            }
                            Err(Error::UnhandledType(setting_type)) => {
                                fx_log_info!(
                                    "setting not available for restore: {:?}",
                                    setting_type
                                );
                                continue;
                            }
                            _ => {
                                fx_log_err!("error during restore for {:?}", component);
                                return Err(AgentError::UnexpectedError);
                            }
                        }
                    } else {
                        return Err(AgentError::UnexpectedError);
                    }
                }
            }
            _ => {
                return Err(AgentError::UnhandledLifespan);
            }
        }

        Ok(())
    }
}
