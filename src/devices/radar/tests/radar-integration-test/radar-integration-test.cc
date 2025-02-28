// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <endian.h>
#include <fuchsia/hardware/radar/llcpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/fdio/fdio.h>
#include <lib/fit/function.h>
#include <lib/sync/completion.h>
#include <lib/zx/channel.h>
#include <stdio.h>

#include <fbl/unique_fd.h>
#include <zxtest/zxtest.h>

namespace {

using BurstReaderProvider = fuchsia_hardware_radar::RadarBurstReaderProvider;
using BurstReader = fuchsia_hardware_radar::RadarBurstReader;
using BurstResult = fuchsia_hardware_radar::wire::RadarBurstReaderOnBurstResult;

constexpr char kRadarDevicePath[] = "/dev/class/radar/000";
constexpr size_t kBurstSize = 23247;

class RadarIntegrationTest : public zxtest::Test {
 public:
  RadarIntegrationTest()
      : loop_(&kAsyncLoopConfigNeverAttachToThread),
        event_handler_(std::make_shared<EventHandler>(this)) {}

  void SetUp() override { loop_.StartThread("radar-integration-test dispatcher"); }

 protected:
  void set_burst_handler(fit::function<void(const BurstResult&)> burst_handler) {
    burst_handler_ = std::move(burst_handler);
  }

  void GetRadarClient(fidl::Client<BurstReader>* out_client) {
    fbl::unique_fd device(open(kRadarDevicePath, O_RDWR));
    ASSERT_TRUE(device.is_valid());

    fidl::WireSyncClient<BurstReaderProvider> provider_client;
    ASSERT_OK(fdio_get_service_handle(device.release(),
                                      provider_client.mutable_channel()->reset_and_get_address()));

    fidl::ClientEnd<BurstReader> client_end;
    fidl::ServerEnd<BurstReader> server_end;
    ASSERT_OK(zx::channel::create(0, &client_end.channel(), &server_end.channel()));

    // Our radar driver currently doesn't support serving multiple clients. Loop on this in case the
    // driver hasn't handled the previous client disconnecting.
    for (;;) {
      auto result = provider_client.Connect(std::move(server_end));
      if (result.ok() && result->result.is_response()) {
        break;
      }
    }

    out_client->Bind(std::move(client_end), loop_.dispatcher(), event_handler_);
  }

  static void CheckBurst(const std::array<uint8_t, kBurstSize>& burst) {
    uint32_t config_id;
    memcpy(&config_id, &burst[0], sizeof(config_id));
    EXPECT_EQ(config_id, 0);

    EXPECT_EQ(burst[4], 30);  // Burst rate in Hz.
    EXPECT_EQ(burst[5], 20);  // Chirps per burst.

    uint16_t chirp_rate_hz;
    memcpy(&chirp_rate_hz, &burst[6], sizeof(chirp_rate_hz));
    EXPECT_EQ(be16toh(chirp_rate_hz), 3000);

    uint16_t samples_per_chirp;
    memcpy(&samples_per_chirp, &burst[8], sizeof(samples_per_chirp));
    EXPECT_EQ(be16toh(samples_per_chirp), 256);

    EXPECT_EQ(burst[10], 0x07);  // RX channel mask.

    uint64_t driver_timestamp, host_timestamp;
    mempcpy(&driver_timestamp, &burst[11], sizeof(driver_timestamp));
    mempcpy(&host_timestamp, &burst[19], sizeof(host_timestamp));
    EXPECT_EQ(driver_timestamp, host_timestamp);
  }

 private:
  class EventHandler : public fidl::WireAsyncEventHandler<BurstReader> {
   public:
    explicit EventHandler(const RadarIntegrationTest* parent) : parent_(*parent) {}

    void OnBurst(fidl::WireResponse<BurstReader::OnBurst>* event) override {
      parent_.OnBurst(event);
    }

    void Unbound(fidl::UnbindInfo info) override {}

   private:
    const RadarIntegrationTest& parent_;
  };

  void OnBurst(fidl::WireResponse<BurstReader::OnBurst>* event) const {
    if (burst_handler_) {
      burst_handler_(event->result);
    }
  }

  async::Loop loop_;
  std::shared_ptr<EventHandler> event_handler_;
  fit::function<void(const BurstResult&)> burst_handler_;
};

TEST_F(RadarIntegrationTest, BurstSize) {
  fidl::Client<BurstReader> client;
  ASSERT_NO_FAILURES(GetRadarClient(&client));

  auto result = client->GetBurstSize_Sync();
  ASSERT_OK(result.status());
  EXPECT_EQ(result->burst_size, kBurstSize);
}

TEST_F(RadarIntegrationTest, Reconnect) {
  fidl::Client<BurstReader> client;
  ASSERT_NO_FAILURES(GetRadarClient(&client));

  {
    const auto result = client->GetBurstSize_Sync();
    ASSERT_OK(result.status());
    EXPECT_EQ(result->burst_size, kBurstSize);
  }

  // Unbind and close our end of the channel. We should eventually be able to reconnect, after the
  // driver has cleaned up after the last client.
  client.WaitForChannel().channel().reset();

  ASSERT_NO_FAILURES(GetRadarClient(&client));

  {
    const auto result = client->GetBurstSize_Sync();
    ASSERT_OK(result.status());
    EXPECT_EQ(result->burst_size, kBurstSize);
  }
}

TEST_F(RadarIntegrationTest, BurstFormat) {
  fidl::Client<BurstReader> client;
  ASSERT_NO_FAILURES(GetRadarClient(&client));

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(kBurstSize, 0, &vmo));

  fidl::FidlAllocator allocator;

  {
    fidl::VectorView<zx::vmo> vmo_dup(allocator, 1);
    ASSERT_OK(vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo_dup[0]));

    fidl::VectorView<uint32_t> vmo_id(allocator, 1);
    vmo_id[0] = 1234;

    const auto result = client->RegisterVmos_Sync(vmo_id, vmo_dup);
    ASSERT_OK(result.status());
    ASSERT_TRUE(result->result.is_response());
  }

  sync_completion_t completion;
  sync_completion_reset(&completion);

  uint32_t received_id = {};

  set_burst_handler([&](const BurstResult& result) {
    if (result.is_response()) {
      received_id = result.response().burst.vmo_id;
      sync_completion_signal(&completion);
    }
  });

  EXPECT_OK(client->StartBursts().status());

  sync_completion_wait(&completion, ZX_TIME_INFINITE);

  EXPECT_OK(client->StopBursts_Sync().status());

  EXPECT_EQ(received_id, 1234);

  std::array<uint8_t, kBurstSize> burst;
  ASSERT_OK(vmo.read(burst.data(), 0, burst.size()));
  ASSERT_NO_FATAL_FAILURES(CheckBurst(burst));

  {
    fidl::VectorView<uint32_t> vmo_id(allocator, 1);
    vmo_id[0] = 1234;

    const auto result = client->UnregisterVmos_Sync(vmo_id);
    ASSERT_OK(result.status());
    ASSERT_TRUE(result->result.is_response());
    ASSERT_EQ(result->result.response().vmos.count(), 1);
    EXPECT_TRUE(result->result.response().vmos[0].is_valid());
  }
}

TEST_F(RadarIntegrationTest, ReadManyBursts) {
  constexpr uint32_t kVmoCount = 10;
  constexpr uint32_t kBurstCount = 303;  // Read for about 10 seconds.

  fidl::Client<BurstReader> client;
  ASSERT_NO_FAILURES(GetRadarClient(&client));

  fidl::FidlAllocator allocator;

  std::vector<zx::vmo> vmos(kVmoCount);

  {
    fidl::VectorView<zx::vmo> vmo_dups(allocator, kVmoCount);
    fidl::VectorView<uint32_t> vmo_ids(allocator, kVmoCount);

    for (size_t i = 0; i < kVmoCount; i++) {
      ASSERT_OK(zx::vmo::create(kBurstSize, 0, &vmos[i]));
      ASSERT_OK(vmos[i].duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo_dups[i]));
      vmo_ids[i] = i;
    }

    const auto result = client->RegisterVmos_Sync(vmo_ids, vmo_dups);
    ASSERT_OK(result.status());
    ASSERT_TRUE(result->result.is_response());
  }

  sync_completion_t completion;
  sync_completion_reset(&completion);

  uint32_t received_burst_count = 0;

  set_burst_handler([&](const BurstResult& result) {
    if (result.is_response()) {
      client->UnlockVmo(result.response().burst.vmo_id);
      if (++received_burst_count >= kBurstCount) {
        sync_completion_signal(&completion);
      }
    }
  });

  EXPECT_OK(client->StartBursts().status());

  sync_completion_wait(&completion, ZX_TIME_INFINITE);

  EXPECT_OK(client->StopBursts_Sync().status());

  EXPECT_GE(received_burst_count, kBurstCount);

  {
    fidl::VectorView<uint32_t> vmo_ids(allocator, kVmoCount);
    for (size_t i = 0; i < kVmoCount; i++) {
      vmo_ids[i] = i;
    }

    const auto result = client->UnregisterVmos_Sync(vmo_ids);
    ASSERT_OK(result.status());
    ASSERT_TRUE(result->result.is_response());
    ASSERT_EQ(result->result.response().vmos.count(), kVmoCount);
    for (size_t i = 0; i < kVmoCount; i++) {
      EXPECT_TRUE(result->result.response().vmos[i].is_valid());
    }
  }
}

}  // namespace
