// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BOARD_DRIVERS_X86_ACPI_STATUS_H_
#define SRC_DEVICES_BOARD_DRIVERS_X86_ACPI_STATUS_H_

#include <lib/fitx/result.h>

#include <acpica/acpi.h>

namespace acpi {

// This is almost a direct copy of zx::status, but wrapping ACPI_STATUS instead of zx_status_t.
// See //zircon/system/ulib/zxc/include/lib/zx/status.h for more information.
using fitx::as_error;
using fitx::error;
using fitx::failed;
using fitx::ok;
using fitx::success;

template <typename... Ts>
class status;

// Specialization of status for returning a single value.
template <typename T>
class LIB_FITX_NODISCARD status<T> : public ::fitx::result<ACPI_STATUS, T> {
  using base = ::fitx::result<ACPI_STATUS, T>;

 public:
  using base::base;

  // Implicit conversion from error<ACPI_STATUS>.
  constexpr status(error<ACPI_STATUS> error) : base{error} {
    // It is invalid to pass AE_OK as an error state. Use acpi::ok() or
    // acpi::success to indicate success. See acpi::make_status for forwarding
    // errors from code that uses ACPI_STATUS.
    if (base::error_value() == AE_OK) {
      __builtin_abort();
    }
  }

  // Returns the underlying error or AE_OK if not in the error state. This
  // accessor simplifies interfacing with code that uses ACPI_STATUS directly.
  constexpr ACPI_STATUS status_value() const {
    return this->is_error() ? base::error_value() : AE_OK;
  }
};

// Specialization of status for empty value type.
template <>
class LIB_FITX_NODISCARD status<> : public ::fitx::result<ACPI_STATUS> {
  using base = ::fitx::result<ACPI_STATUS>;

 public:
  using base::base;

  // Implicit conversion from error<ACPI_STATUS>.
  constexpr status(error<ACPI_STATUS> error) : base{error} {
    // It is invalid to pass AE_OK as an error state. Use acpi::ok() or
    // acpi::success to indicate success. See acpi::make_status for forwarding
    // errors from code that uses ACPI_STATUS.
    if (base::error_value() == AE_OK) {
      __builtin_abort();
    }
  }

  // Returns the underlying error or AE_OK if not in the error state. This
  // accessor simplifies interfacing with code that uses ACPI_STATUS directly.
  constexpr ACPI_STATUS status_value() const {
    return this->is_error() ? base::error_value() : AE_OK;
  }
};

// Simplified alias of acpi::error<ACPI_STATUS>.
using error_status = error<ACPI_STATUS>;

// Utility to make a status-only acpi::status<> from a ACPI_STATUS error.
//
// A status-only acpi::status<> is one with an empty value set. It may contain
// either a status value that represents the error (i.e. not AE_OK) or a
// valueless success state. This utility automatically handles the distinction
// to make interop with older code easier.
//
// Example usage:
//
//   // Legacy method returning ACPI_STATUS.
//   ACPI_STATUS ConsumeValues(Value* values, size_t length);
//
//   // Newer method that interops with the legacy method.
//   acpi::status<> ConsumeValues(std::array<Value, kSize>* values) {
//     if (values == nullptr) {
//       return acpi::error_status(AE_ERR_INVALID_ARGS);
//     }
//     return acpi::make_status(ConsumeValues(values->data(), values->length()));
//   }
//
constexpr status<> make_status(ACPI_STATUS status) {
  if (status == AE_OK) {
    return ok();
  }
  return error_status{status};
}

}  // namespace acpi

#endif  // SRC_DEVICES_BOARD_DRIVERS_X86_ACPI_STATUS_H_
