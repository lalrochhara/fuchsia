// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/bin/root_presenter/focus_dispatcher.h"

#include <fuchsia/ui/focus/cpp/fidl.h>
#include <fuchsia/ui/keyboard/focus/cpp/fidl.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/status.h>

namespace root_presenter {

using fuchsia::ui::focus::FocusChain;
using fuchsia::ui::focus::FocusChainListener;
using fuchsia::ui::focus::FocusChainListenerRegistry;
using fuchsia::ui::keyboard::focus::Controller;
using sys::ServiceDirectory;

FocusDispatcher::FocusDispatcher(const std::shared_ptr<ServiceDirectory>& svc) {
  // Connect to `fuchsia.ui.keyboard.focus.Controller`.
  keyboard_focus_ctl_ = svc->Connect<Controller>();
  keyboard_focus_ctl_.set_error_handler([](zx_status_t status) {
    FX_LOGS(WARNING) << "Unable to connect to fuchsia.ui.keyboard.focus.Controller: "
                     << zx_status_get_string(status);
  });

  // Connect to `fuchsia.ui.focus.FocusChainListenerRegistry`, then send it
  // a client-side handle to `fuchsia.ui.focus.FocusChainListener`.
  focus_chain_listener_registry_ = svc->Connect<FocusChainListenerRegistry>();
  focus_chain_listener_registry_.set_error_handler([](zx_status_t status) {
    FX_LOGS(WARNING) << "Unable to connect to fuchsia.ui.focus.FocusChainListenerRegistry: "
                     << zx_status_get_string(status);
  });
  auto handle = focus_chain_listeners_.AddBinding(this);
  focus_chain_listener_registry_->Register(handle.Bind());
}

void FocusDispatcher::OnFocusChange(FocusChain new_focus_chain,
                                    FocusChainListener::OnFocusChangeCallback callback) {
  if (new_focus_chain.has_focus_chain()) {
    auto& focus_chain = new_focus_chain.focus_chain();
    if (focus_chain.empty()) {
      FX_LOGS(ERROR) << "OnFocusChange: empty focus chain - should not happen";
    } else {
      auto& last_view_ref = focus_chain.back();

      if (keyboard_focus_ctl_) {
        keyboard_focus_ctl_->Notify(fidl::Clone(last_view_ref), [] {
          FX_LOGS(DEBUG) << "FocusDispatcher::OnFocusChange: notify succeeded.";
        });
      }
    }
  }
  // Callback is invoked regardless of whether `Notify` succeeds, and
  // asynchronouly with Controller.Notify above.
  callback();
}

}  // namespace root_presenter
