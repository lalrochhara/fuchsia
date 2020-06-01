// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/ui/input/cpp/fidl.h>
#include <fuchsia/ui/pointerinjector/cpp/fidl.h>

#include <gtest/gtest.h>

#include "src/ui/scenic/lib/input/helper.h"

namespace lib_ui_input_tests {
namespace {

static const glm::mat3 kIdentity{};
static constexpr uint32_t kDeviceId = 0;

using Phase = fuchsia::ui::pointerinjector::EventPhase;
using DeviceType = fuchsia::ui::pointerinjector::DeviceType;

TEST(PointerEventTranslationTest, ReversePointerTraceHACK) {
  const float high = -3.40282e+38;
  const float low = 2.22222e+06;

  const trace_flow_id_t trace_id = scenic_impl::input::PointerTraceHACK(high, low);
  const auto [rhigh, rlow] = scenic_impl::input::ReversePointerTraceHACK(trace_id);

  EXPECT_EQ(rhigh, high);
  EXPECT_EQ(rlow, low);
}

TEST(PointerEventTranslationTest, Add) {
  fuchsia::ui::pointerinjector::Event event;
  event.set_timestamp(1);
  {
    fuchsia::ui::pointerinjector::PointerSample pointer_sample;
    pointer_sample.set_pointer_id(2);
    pointer_sample.set_position_in_viewport({3, 4});
    pointer_sample.set_phase(Phase::ADD);
    fuchsia::ui::pointerinjector::Data data;
    data.set_pointer_sample(std::move(pointer_sample));
    event.set_data(std::move(data));
  }

  std::vector<fuchsia::ui::input::PointerEvent> results =
      scenic_impl::input::PointerInjectorEventToGfxPointerEvent(
          event, kDeviceId, /*viewport_to_context_transform*/ kIdentity);

  ASSERT_EQ(results.size(), 2u);
  EXPECT_EQ(results[0].phase, fuchsia::ui::input::PointerEventPhase::ADD);
  EXPECT_EQ(results[0].device_id, kDeviceId);
  EXPECT_EQ(results[0].event_time, 1u);
  EXPECT_EQ(results[0].pointer_id, 2u);
  EXPECT_EQ(results[0].x, 3);
  EXPECT_EQ(results[0].y, 4);

  EXPECT_EQ(results[1].phase, fuchsia::ui::input::PointerEventPhase::DOWN);
  EXPECT_EQ(results[1].device_id, kDeviceId);
  EXPECT_EQ(results[1].event_time, 1u);
  EXPECT_EQ(results[1].pointer_id, 2u);
  EXPECT_EQ(results[1].x, 3);
  EXPECT_EQ(results[1].y, 4);
}

TEST(PointerEventTranslationTest, Change) {
  fuchsia::ui::pointerinjector::Event event;
  event.set_timestamp(1);
  {
    fuchsia::ui::pointerinjector::PointerSample pointer_sample;
    pointer_sample.set_pointer_id(2);
    pointer_sample.set_position_in_viewport({3, 4});
    pointer_sample.set_phase(Phase::CHANGE);
    fuchsia::ui::pointerinjector::Data data;
    data.set_pointer_sample(std::move(pointer_sample));
    event.set_data(std::move(data));
  }

  std::vector<fuchsia::ui::input::PointerEvent> results =
      scenic_impl::input::PointerInjectorEventToGfxPointerEvent(
          event, kDeviceId, /*viewport_to_context_transform*/ kIdentity);

  ASSERT_EQ(results.size(), 1u);
  EXPECT_EQ(results[0].phase, fuchsia::ui::input::PointerEventPhase::MOVE);
  EXPECT_EQ(results[0].device_id, kDeviceId);
  EXPECT_EQ(results[0].event_time, 1u);
  EXPECT_EQ(results[0].pointer_id, 2u);
  EXPECT_EQ(results[0].x, 3);
  EXPECT_EQ(results[0].y, 4);
}

TEST(PointerEventTranslationTest, Remove) {
  fuchsia::ui::pointerinjector::Event event;
  event.set_timestamp(1);
  {
    fuchsia::ui::pointerinjector::PointerSample pointer_sample;
    pointer_sample.set_pointer_id(2);
    pointer_sample.set_position_in_viewport({3, 4});
    pointer_sample.set_phase(Phase::REMOVE);
    fuchsia::ui::pointerinjector::Data data;
    data.set_pointer_sample(std::move(pointer_sample));
    event.set_data(std::move(data));
  }

  std::vector<fuchsia::ui::input::PointerEvent> results =
      scenic_impl::input::PointerInjectorEventToGfxPointerEvent(
          event, kDeviceId, /*viewport_to_context_transform*/ kIdentity);

  ASSERT_EQ(results.size(), 2u);
  EXPECT_EQ(results[0].phase, fuchsia::ui::input::PointerEventPhase::UP);
  EXPECT_EQ(results[0].device_id, kDeviceId);
  EXPECT_EQ(results[0].event_time, 1u);
  EXPECT_EQ(results[0].pointer_id, 2u);
  EXPECT_EQ(results[0].x, 3);
  EXPECT_EQ(results[0].y, 4);

  EXPECT_EQ(results[1].phase, fuchsia::ui::input::PointerEventPhase::REMOVE);
  EXPECT_EQ(results[1].device_id, kDeviceId);
  EXPECT_EQ(results[1].event_time, 1u);
  EXPECT_EQ(results[1].pointer_id, 2u);
  EXPECT_EQ(results[1].x, 3);
  EXPECT_EQ(results[1].y, 4);
}

TEST(PointerEventTranslationTest, Cancel) {
  fuchsia::ui::pointerinjector::Event event;
  event.set_timestamp(1);
  {
    fuchsia::ui::pointerinjector::PointerSample pointer_sample;
    pointer_sample.set_pointer_id(2);
    pointer_sample.set_position_in_viewport({3, 4});
    pointer_sample.set_phase(Phase::CANCEL);
    fuchsia::ui::pointerinjector::Data data;
    data.set_pointer_sample(std::move(pointer_sample));
    event.set_data(std::move(data));
  }

  std::vector<fuchsia::ui::input::PointerEvent> results =
      scenic_impl::input::PointerInjectorEventToGfxPointerEvent(
          event, kDeviceId, /*viewport_to_context_transform*/ kIdentity);

  ASSERT_EQ(results.size(), 1u);
  EXPECT_EQ(results[0].phase, fuchsia::ui::input::PointerEventPhase::CANCEL);
  EXPECT_EQ(results[0].device_id, kDeviceId);
  EXPECT_EQ(results[0].event_time, 1u);
  EXPECT_EQ(results[0].pointer_id, 2u);
  EXPECT_EQ(results[0].x, 3);
  EXPECT_EQ(results[0].y, 4);
}

TEST(PointerEventTranslationTest, TraceFlowId) {
  constexpr uint32_t device_id = 0;

  fuchsia::ui::pointerinjector::Event event;
  event.set_timestamp(1);
  {
    fuchsia::ui::pointerinjector::PointerSample pointer_sample;
    pointer_sample.set_pointer_id(2);
    pointer_sample.set_position_in_viewport({3, 4});
    pointer_sample.set_phase(Phase::ADD);
    fuchsia::ui::pointerinjector::Data data;
    data.set_pointer_sample(std::move(pointer_sample));
    event.set_data(std::move(data));
  }

  // Create a trace id with some high bits and low bits.
  constexpr float high = 7;
  constexpr float low = 5;
  event.set_trace_flow_id(scenic_impl::input::PointerTraceHACK(high, low));

  std::vector<fuchsia::ui::input::PointerEvent> results =
      scenic_impl::input::PointerInjectorEventToGfxPointerEvent(
          event, kDeviceId, /*viewport_to_context_transform*/ kIdentity);

  ASSERT_EQ(results.size(), 2u);
  EXPECT_EQ(results[0].radius_minor, low);
  EXPECT_EQ(results[0].radius_major, high);
  EXPECT_EQ(results[1].radius_minor, low);
  EXPECT_EQ(results[1].radius_major, high);
}

}  // namespace
}  // namespace lib_ui_input_tests
