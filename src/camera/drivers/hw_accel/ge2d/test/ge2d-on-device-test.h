// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CAMERA_DRIVERS_HW_ACCEL_GE2D_TEST_GE2D_ON_DEVICE_TEST_H_
#define SRC_CAMERA_DRIVERS_HW_ACCEL_GE2D_TEST_GE2D_ON_DEVICE_TEST_H_

#include <memory>

#include <zxtest/zxtest.h>

namespace ge2d {
// |Ge2dDeviceTester| is spawned by the driver in |ge2d.cc|
class Ge2dDevice;

class Ge2dDeviceTester : public zxtest::Test {
 public:
  static zx_status_t RunTests(Ge2dDevice* ge2d);

 protected:
  // Setup & TearDown
  void SetUp() override;
  void TearDown() override;
};

}  // namespace ge2d

#endif  // SRC_CAMERA_DRIVERS_HW_ACCEL_GE2D_TEST_GE2D_ON_DEVICE_TEST_H_
