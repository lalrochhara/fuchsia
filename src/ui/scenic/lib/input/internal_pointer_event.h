// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_INPUT_INTERNAL_POINTER_EVENT_H_
#define SRC_UI_SCENIC_LIB_INPUT_INTERNAL_POINTER_EVENT_H_

#include <zircon/types.h>

#include <array>
#include <optional>

#include "src/ui/lib/glm_workaround/glm_workaround.h"

namespace scenic_impl::input {

using Mat3ColumnMajorArray = std::array<float, 9>;

// Possible states the pointer can be in.
// TODO(fxbug.dev/53316): Remove UP and DOWN phases when old input injection API is removed.
enum Phase { kInvalid, kAdd, kDown, kChange, kUp, kRemove, kCancel };

// Extents define an axis-aligned rectangle in 2D space.
struct Extents {
  // Minimum (top left) corner.
  glm::vec2 min = glm::vec2(0);
  // Maximum (bottom right) corner.
  glm::vec2 max = glm::vec2(0);
  Extents() = default;
  Extents(std::array<std::array<float, 2>, 2> extents) {
    min = {extents[0][0], extents[0][1]};
    max = {extents[1][0], extents[1][1]};
  }

  // Used to check for exact equality in TouchSource
  inline bool operator==(const Extents& other) const {
    return min == other.min && max == other.max;
  }
};

// Viewport defines an arbitrary rectangle in the space of the injector context.
// The Viewport is effectively a touchscreen abstraction that can be relayed to clients
// in their local space.
struct Viewport {
  // A rectangle describing the axis-aligned edges of the Viewport in Viewport-local space.
  Extents extents;
  // A transform defining the Viewport in relation to a context (a View).
  glm::mat4 context_from_viewport_transform = glm::mat4(1.f);

  // A 2D transform defining the Viewport in relation to a receiver (a View), in column-major order.
  // Must be set when handed to GestureContender (since that's when the receiver is determined).
  std::optional<Mat3ColumnMajorArray> receiver_from_viewport_transform;

  // Used to check for exact equality in TouchSource
  inline bool operator==(const Viewport& other) const {
    return extents == other.extents &&
           context_from_viewport_transform == other.context_from_viewport_transform;
  }
  inline bool operator!=(const Viewport& other) const { return !(*this == other); }
};

// Pointer event representation to be used internally, uncoupled from FIDL types.
struct InternalPointerEvent {
  zx_time_t timestamp = 0;
  // Id of the injection device.
  // TODO(fxbug.dev/53352): This is currently only unique per Injector. Make globally unique.
  uint32_t device_id = 0u;
  // Id of the pointer this event belongs to (== a finger on a touchscreen).
  uint32_t pointer_id = 0u;
  // Current event state.
  Phase phase = Phase::kInvalid;
  // Reference to the context the event was injected from (a View).
  zx_koid_t context = ZX_KOID_INVALID;
  // Reference to the target the event was injected into (a View).
  zx_koid_t target = ZX_KOID_INVALID;
  // The Viewport this event was injected with.
  Viewport viewport;
  // Coordinates in Viewport space. Pointer events do not necessarily need to stay within the
  // Viewport's extents, but are counted as a hit test miss when outside.
  glm::vec2 position_in_viewport = glm::vec2(0, 0);
  // Integer describing mouse buttons. From gfx SessionListener API.
  uint32_t buttons = 0;
};

}  // namespace scenic_impl::input

#endif  // SRC_UI_SCENIC_LIB_INPUT_INTERNAL_POINTER_EVENT_H_
