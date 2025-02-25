// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/bluetooth/core/bt-host/fidl/low_energy_central_server.h"

#include <cstddef>

#include <gmock/gmock.h>
#include <measure_tape/hlcpp/hlcpp_measure_tape_for_peer.h>

#include "adapter_test_fixture.h"
#include "fuchsia/bluetooth/gatt/cpp/fidl.h"
#include "fuchsia/bluetooth/le/cpp/fidl.h"
#include "lib/fidl/cpp/interface_request.h"
#include "src/connectivity/bluetooth/core/bt-host/common/status.h"
#include "src/connectivity/bluetooth/core/bt-host/fidl/helpers.h"
#include "src/connectivity/bluetooth/core/bt-host/sm/types.h"
#include "src/connectivity/bluetooth/core/bt-host/testing/fake_controller.h"
#include "src/connectivity/bluetooth/core/bt-host/testing/fake_peer.h"

namespace bthost {
namespace {

namespace fble = fuchsia::bluetooth::le;
namespace fgatt = fuchsia::bluetooth::gatt;

const bt::DeviceAddress kTestAddr(bt::DeviceAddress::Type::kLEPublic, {0x01, 0, 0, 0, 0, 0});
const size_t kLEMaxNumPackets = 10;
const bt::hci::DataBufferInfo kLEDataBufferInfo(bt::hci::kMaxACLPayloadSize, kLEMaxNumPackets);

fble::ScanOptions ScanOptionsWithEmptyFilter() {
  fble::ScanOptions options;
  fble::Filter filter;
  std::vector<fble::Filter> filters;
  filters.emplace_back(std::move(filter));
  options.set_filters(std::move(filters));
  return options;
}

size_t MaxPeersPerScanResultWatcherChannel(const bt::gap::Peer& peer) {
  const size_t kPeerSize =
      measure_tape::fuchsia::bluetooth::le::Measure(fidl_helpers::PeerToFidlLe(peer)).num_bytes;
  const size_t kVectorOverhead = sizeof(fidl_message_header_t) + sizeof(fidl_vector_t);
  const size_t kMaxBytes = ZX_CHANNEL_MAX_MSG_BYTES - kVectorOverhead;
  return kMaxBytes / kPeerSize;
}

using TestingBase = bthost::testing::AdapterTestFixture;

class FIDL_LowEnergyCentralServerTest : public TestingBase {
 public:
  FIDL_LowEnergyCentralServerTest() = default;
  ~FIDL_LowEnergyCentralServerTest() override = default;

  void SetUp() override {
    AdapterTestFixture::SetUp();

    // Create a LowEnergyCentralServer and bind it to a local client.
    fidl::InterfaceHandle<fble::Central> handle;
    gatt_ = take_gatt();
    server_ = std::make_unique<LowEnergyCentralServer>(adapter(), handle.NewRequest(),
                                                       gatt_->AsWeakPtr());
    proxy_.Bind(std::move(handle));

    bt::testing::FakeController::Settings settings;
    settings.ApplyLegacyLEConfig();
    test_device()->set_settings(settings);
  }

  void TearDown() override {
    RunLoopUntilIdle();

    proxy_ = nullptr;
    server_ = nullptr;
    gatt_ = nullptr;

    RunLoopUntilIdle();
    AdapterTestFixture::TearDown();
  }

 protected:
  // Returns true if the given gatt.Client handle was closed after the event
  // loop finishes processing. Returns false if the handle was not closed.
  // Ownership of |handle| remains with the caller when this method returns.
  bool IsClientHandleClosedAfterLoop(fidl::InterfaceHandle<fgatt::Client>* handle) {
    ZX_ASSERT(handle);

    fgatt::ClientPtr proxy;
    proxy.Bind(std::move(*handle));

    bool closed = false;
    proxy.set_error_handler([&](zx_status_t s) {
      EXPECT_EQ(ZX_ERR_PEER_CLOSED, s);
      closed = true;
    });
    RunLoopUntilIdle();

    *handle = proxy.Unbind();
    return closed;
  }

  // Destroys the FIDL server. The le.Central proxy will be shut down and
  // subsequent calls to `server()` will return nullptr.
  void DestroyServer() { server_ = nullptr; }

  LowEnergyCentralServer* server() const { return server_.get(); }
  fuchsia::bluetooth::le::Central* central_proxy() const { return proxy_.get(); }

 private:
  std::unique_ptr<LowEnergyCentralServer> server_;
  fble::CentralPtr proxy_;
  std::unique_ptr<bt::gatt::GATT> gatt_;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(FIDL_LowEnergyCentralServerTest);
};

// Tests that connecting to a peripheral with LowEnergyConnectionOptions.bondable_mode unset results
// in a bondable connection ref being stored in LowEnergyConnectionManager
TEST_F(FIDL_LowEnergyCentralServerTest, ConnectDefaultResultsBondableConnectionRef) {
  auto* const peer = adapter()->peer_cache()->NewPeer(kTestAddr, /*connectable=*/true);
  ASSERT_TRUE(peer);

  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kTestAddr));

  fble::ConnectionOptions options;

  fidl::InterfaceHandle<fuchsia::bluetooth::gatt::Client> gatt_client;
  fidl::InterfaceRequest<fuchsia::bluetooth::gatt::Client> gatt_client_req =
      gatt_client.NewRequest();

  auto status =
      fidl_helpers::NewFidlError(fuchsia::bluetooth::ErrorCode::BAD_STATE, "this should change");
  auto callback = [&status](::fuchsia::bluetooth::Status cb_status) {
    ASSERT_EQ(cb_status.error, nullptr);
    status = std::move(cb_status);
  };
  central_proxy()->ConnectPeripheral(peer->identifier().ToString(), std::move(options),
                                     std::move(gatt_client_req), callback);
  ASSERT_FALSE(server()->FindConnectionForTesting(peer->identifier()));
  RunLoopUntilIdle();
  auto conn_ref = server()->FindConnectionForTesting(peer->identifier());
  ASSERT_EQ(status.error, nullptr);
  ASSERT_TRUE(conn_ref.has_value());
  ASSERT_TRUE(conn_ref.value());
  ASSERT_EQ(conn_ref.value()->bondable_mode(), bt::sm::BondableMode::Bondable);
}

// Tests that setting LowEnergyConnectionOptions.bondable_mode to true and connecting to a peer in
// bondable mode results in a bondable connection ref being stored in LowEnergyConnectionManager
TEST_F(FIDL_LowEnergyCentralServerTest, ConnectBondableResultsBondableConnectionRef) {
  auto* const peer = adapter()->peer_cache()->NewPeer(kTestAddr, /*connectable=*/true);
  ASSERT_TRUE(peer);

  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kTestAddr));

  fble::ConnectionOptions options;
  options.set_bondable_mode(true);

  fidl::InterfaceHandle<fuchsia::bluetooth::gatt::Client> gatt_client;
  fidl::InterfaceRequest<fuchsia::bluetooth::gatt::Client> gatt_client_req =
      gatt_client.NewRequest();

  auto status =
      fidl_helpers::NewFidlError(fuchsia::bluetooth::ErrorCode::BAD_STATE, "this should change");
  auto callback = [&status](::fuchsia::bluetooth::Status cb_status) {
    ASSERT_EQ(cb_status.error, nullptr);
    status = std::move(cb_status);
  };
  central_proxy()->ConnectPeripheral(peer->identifier().ToString(), std::move(options),
                                     std::move(gatt_client_req), callback);
  ASSERT_FALSE(server()->FindConnectionForTesting(peer->identifier()));
  RunLoopUntilIdle();
  auto conn_ref = server()->FindConnectionForTesting(peer->identifier());
  ASSERT_EQ(status.error, nullptr);
  ASSERT_TRUE(conn_ref.has_value());
  ASSERT_TRUE(conn_ref.value());
  ASSERT_EQ(conn_ref.value()->bondable_mode(), bt::sm::BondableMode::Bondable);
}

// Tests that setting LowEnergyConnectionOptions.bondable_mode to false and connecting to a peer
// results in a non-bondable connection ref being stored in LowEnergyConnectionManager.
TEST_F(FIDL_LowEnergyCentralServerTest, ConnectNonBondableResultsNonBondableConnectionRef) {
  auto* const peer = adapter()->peer_cache()->NewPeer(kTestAddr, /*connectable=*/true);
  ASSERT_TRUE(peer);

  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kTestAddr));

  fble::ConnectionOptions options;
  options.set_bondable_mode(false);

  fidl::InterfaceHandle<fuchsia::bluetooth::gatt::Client> gatt_client;
  fidl::InterfaceRequest<fuchsia::bluetooth::gatt::Client> gatt_client_req =
      gatt_client.NewRequest();

  auto status =
      fidl_helpers::NewFidlError(fuchsia::bluetooth::ErrorCode::BAD_STATE, "this should change");
  auto callback = [&status](::fuchsia::bluetooth::Status cb_status) {
    ASSERT_EQ(cb_status.error, nullptr);
    status = std::move(cb_status);
  };
  central_proxy()->ConnectPeripheral(peer->identifier().ToString(), std::move(options),
                                     std::move(gatt_client_req), callback);
  ASSERT_FALSE(server()->FindConnectionForTesting(peer->identifier()));
  RunLoopUntilIdle();
  auto conn_ref = server()->FindConnectionForTesting(peer->identifier());
  ASSERT_EQ(status.error, nullptr);
  ASSERT_TRUE(conn_ref.has_value());
  ASSERT_TRUE(conn_ref.value());
  ASSERT_EQ(conn_ref.value()->bondable_mode(), bt::sm::BondableMode::NonBondable);
}

TEST_F(FIDL_LowEnergyCentralServerTest, DisconnectUnconnectedPeripheralReturnsSuccess) {
  auto status =
      fidl_helpers::NewFidlError(fuchsia::bluetooth::ErrorCode::BAD_STATE, "this should change");
  auto callback = [&status](::fuchsia::bluetooth::Status cb_status) {
    status = std::move(cb_status);
  };
  central_proxy()->DisconnectPeripheral(bt::PeerId(1).ToString(), std::move(callback));
  RunLoopUntilIdle();
  EXPECT_EQ(status.error, nullptr);
}

TEST_F(FIDL_LowEnergyCentralServerTest, FailedConnectionCleanedUp) {
  auto* const peer = adapter()->peer_cache()->NewPeer(kTestAddr, /*connectable=*/true);
  ASSERT_TRUE(peer);

  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kTestAddr));

  fble::ConnectionOptions options;

  fidl::InterfaceHandle<fuchsia::bluetooth::gatt::Client> gatt_client;
  fidl::InterfaceRequest<fuchsia::bluetooth::gatt::Client> gatt_client_req =
      gatt_client.NewRequest();

  fuchsia::bluetooth::Status status;
  auto callback = [&status](::fuchsia::bluetooth::Status cb_status) {
    status = std::move(cb_status);
  };

  test_device()->SetDefaultCommandStatus(bt::hci::kReadRemoteVersionInfo,
                                         bt::hci::StatusCode::kConnectionLimitExceeded);

  ASSERT_FALSE(server()->FindConnectionForTesting(peer->identifier()).has_value());
  central_proxy()->ConnectPeripheral(peer->identifier().ToString(), std::move(options),
                                     std::move(gatt_client_req), callback);
  RunLoopUntilIdle();
  auto conn = server()->FindConnectionForTesting(peer->identifier());
  EXPECT_NE(status.error, nullptr);
  EXPECT_FALSE(conn.has_value());
}

TEST_F(FIDL_LowEnergyCentralServerTest, ConnectPeripheralAlreadyConnectedInLecm) {
  auto* const peer = adapter()->peer_cache()->NewPeer(kTestAddr, /*connectable=*/true);
  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kTestAddr));

  std::unique_ptr<bt::gap::LowEnergyConnectionHandle> le_conn;
  adapter()->le()->Connect(
      peer->identifier(),
      [&le_conn](auto result) {
        ASSERT_TRUE(result.is_ok());
        le_conn = result.take_value();
      },
      bt::gap::LowEnergyConnectionOptions());
  RunLoopUntilIdle();
  ASSERT_TRUE(le_conn);
  ASSERT_FALSE(server()->FindConnectionForTesting(peer->identifier()).has_value());

  fuchsia::bluetooth::Status status;
  auto callback = [&status](::fuchsia::bluetooth::Status cb_status) {
    status = std::move(cb_status);
  };

  fble::ConnectionOptions options;
  fidl::InterfaceHandle<fuchsia::bluetooth::gatt::Client> gatt_client;
  fidl::InterfaceRequest<fuchsia::bluetooth::gatt::Client> gatt_client_req =
      gatt_client.NewRequest();
  central_proxy()->ConnectPeripheral(peer->identifier().ToString(), std::move(options),
                                     std::move(gatt_client_req), callback);
  RunLoopUntilIdle();
  EXPECT_EQ(status.error, nullptr);
  auto server_conn = server()->FindConnectionForTesting(peer->identifier());
  ASSERT_TRUE(server_conn.has_value());
  EXPECT_NE(server_conn.value(), nullptr);
}

TEST_F(FIDL_LowEnergyCentralServerTest, ConnectPeripheralUnknownPeer) {
  fuchsia::bluetooth::Status status;
  auto callback = [&status](::fuchsia::bluetooth::Status cb_status) {
    status = std::move(cb_status);
  };

  const bt::PeerId peer_id(1);

  fble::ConnectionOptions options;
  fidl::InterfaceHandle<fuchsia::bluetooth::gatt::Client> gatt_client;
  fidl::InterfaceRequest<fuchsia::bluetooth::gatt::Client> gatt_client_req =
      gatt_client.NewRequest();
  central_proxy()->ConnectPeripheral(peer_id.ToString(), std::move(options),
                                     std::move(gatt_client_req), callback);
  RunLoopUntilIdle();
  ASSERT_TRUE(status.error);
  EXPECT_EQ(status.error->error_code, fuchsia::bluetooth::ErrorCode::NOT_FOUND);
  auto server_conn = server()->FindConnectionForTesting(peer_id);
  EXPECT_FALSE(server_conn.has_value());
}

TEST_F(FIDL_LowEnergyCentralServerTest, DisconnectPeripheralClosesCorrectGattHandle) {
  const bt::DeviceAddress kAddr1 = kTestAddr;
  const bt::DeviceAddress kAddr2(bt::DeviceAddress::Type::kLEPublic, {2, 0, 0, 0, 0, 0});
  auto* const peer1 = adapter()->peer_cache()->NewPeer(kAddr1, /*connectable=*/true);
  auto* const peer2 = adapter()->peer_cache()->NewPeer(kAddr2, /*connectable=*/true);

  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kAddr1));
  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kAddr2));

  // Establish two connections.
  fidl::InterfaceHandle<fgatt::Client> handle1, handle2;
  central_proxy()->ConnectPeripheral(peer1->identifier().ToString(), fble::ConnectionOptions{},
                                     handle1.NewRequest(), [](auto) {});
  central_proxy()->ConnectPeripheral(peer2->identifier().ToString(), fble::ConnectionOptions{},
                                     handle2.NewRequest(), [](auto) {});
  RunLoopUntilIdle();
  ASSERT_TRUE(server()->FindConnectionForTesting(peer1->identifier()));
  ASSERT_TRUE(server()->FindConnectionForTesting(peer2->identifier()));
  EXPECT_FALSE(IsClientHandleClosedAfterLoop(&handle1));
  EXPECT_FALSE(IsClientHandleClosedAfterLoop(&handle2));

  // Disconnect peer1. Only its gatt.Client handle should close.
  central_proxy()->DisconnectPeripheral(peer1->identifier().ToString(), [](auto) {});
  EXPECT_TRUE(IsClientHandleClosedAfterLoop(&handle1));
  EXPECT_FALSE(IsClientHandleClosedAfterLoop(&handle2));

  // Disconnect peer2. Its handle should close now.
  central_proxy()->DisconnectPeripheral(peer2->identifier().ToString(), [](auto) {});
  EXPECT_TRUE(IsClientHandleClosedAfterLoop(&handle2));
}

TEST_F(FIDL_LowEnergyCentralServerTest, PeerDisconnectClosesCorrectHandle) {
  const bt::DeviceAddress kAddr1 = kTestAddr;
  const bt::DeviceAddress kAddr2(bt::DeviceAddress::Type::kLEPublic, {2, 0, 0, 0, 0, 0});
  auto* const peer1 = adapter()->peer_cache()->NewPeer(kAddr1, /*connectable=*/true);
  auto* const peer2 = adapter()->peer_cache()->NewPeer(kAddr2, /*connectable=*/true);

  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kAddr1));
  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kAddr2));

  // Establish two connections.
  fidl::InterfaceHandle<fgatt::Client> handle1, handle2;
  central_proxy()->ConnectPeripheral(peer1->identifier().ToString(), fble::ConnectionOptions{},
                                     handle1.NewRequest(), [](auto) {});
  central_proxy()->ConnectPeripheral(peer2->identifier().ToString(), fble::ConnectionOptions{},
                                     handle2.NewRequest(), [](auto) {});
  RunLoopUntilIdle();
  ASSERT_TRUE(server()->FindConnectionForTesting(peer1->identifier()));
  ASSERT_TRUE(server()->FindConnectionForTesting(peer2->identifier()));
  EXPECT_FALSE(IsClientHandleClosedAfterLoop(&handle1));
  EXPECT_FALSE(IsClientHandleClosedAfterLoop(&handle2));

  // Disconnect peer1. Only its gatt.Client handle should close.
  test_device()->Disconnect(kAddr1);
  EXPECT_TRUE(IsClientHandleClosedAfterLoop(&handle1));
  EXPECT_FALSE(IsClientHandleClosedAfterLoop(&handle2));

  // Disconnect peer2. Its handle should close now.
  test_device()->Disconnect(kAddr2);
  EXPECT_TRUE(IsClientHandleClosedAfterLoop(&handle2));
}

TEST_F(FIDL_LowEnergyCentralServerTest, ClosingCentralHandleClosesAssociatedGattClientHandles) {
  const bt::DeviceAddress kAddr1 = kTestAddr;
  const bt::DeviceAddress kAddr2(bt::DeviceAddress::Type::kLEPublic, {2, 0, 0, 0, 0, 0});
  auto* const peer1 = adapter()->peer_cache()->NewPeer(kAddr1, /*connectable=*/true);
  auto* const peer2 = adapter()->peer_cache()->NewPeer(kAddr2, /*connectable=*/true);

  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kAddr1));
  test_device()->AddPeer(std::make_unique<bt::testing::FakePeer>(kAddr2));

  // Establish two connections.
  fidl::InterfaceHandle<fgatt::Client> handle1, handle2;
  central_proxy()->ConnectPeripheral(peer1->identifier().ToString(), fble::ConnectionOptions{},
                                     handle1.NewRequest(), [](auto) {});
  central_proxy()->ConnectPeripheral(peer2->identifier().ToString(), fble::ConnectionOptions{},
                                     handle2.NewRequest(), [](auto) {});
  RunLoopUntilIdle();
  ASSERT_TRUE(server()->FindConnectionForTesting(peer1->identifier()));
  ASSERT_TRUE(server()->FindConnectionForTesting(peer2->identifier()));
  EXPECT_FALSE(IsClientHandleClosedAfterLoop(&handle1));
  EXPECT_FALSE(IsClientHandleClosedAfterLoop(&handle2));

  DestroyServer();
  EXPECT_TRUE(IsClientHandleClosedAfterLoop(&handle1));
  EXPECT_TRUE(IsClientHandleClosedAfterLoop(&handle2));
}

TEST_F(FIDL_LowEnergyCentralServerTest, ScanWithEmptyScanOptionsFails) {
  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();

  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> result_watcher_epitaph;
  result_watcher_client.set_error_handler(
      [&](zx_status_t epitaph) { result_watcher_epitaph = epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(fble::ScanOptions(), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);
  ASSERT_TRUE(result_watcher_epitaph.has_value());
  EXPECT_EQ(result_watcher_epitaph.value(), ZX_ERR_INVALID_ARGS);
}

TEST_F(FIDL_LowEnergyCentralServerTest, ScanWithNoFiltersFails) {
  fble::ScanOptions options;
  std::vector<fble::Filter> filters;
  options.set_filters(std::move(filters));

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();

  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> result_watcher_epitaph;
  result_watcher_client.set_error_handler(
      [&](zx_status_t epitaph) { result_watcher_epitaph = epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(std::move(options), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);
  ASSERT_TRUE(result_watcher_epitaph.has_value());
  EXPECT_EQ(result_watcher_epitaph.value(), ZX_ERR_INVALID_ARGS);
}

TEST_F(FIDL_LowEnergyCentralServerTest, ScanReceivesPeerPreviouslyAddedToPeerCache) {
  bt::gap::Peer* peer = adapter()->peer_cache()->NewPeer(kTestAddr, /*connectable=*/false);

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> epitaph;
  result_watcher_client.set_error_handler([&](zx_status_t cb_epitaph) { epitaph = cb_epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });

  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped);
  EXPECT_FALSE(epitaph);

  std::optional<std::vector<fble::Peer>> peers;
  result_watcher_client->Watch(
      [&](std::vector<fble::Peer> updated) { peers = std::move(updated); });
  RunLoopUntilIdle();
  ASSERT_TRUE(peers.has_value());
  ASSERT_EQ(peers->size(), 1u);
  ASSERT_TRUE(peers->front().has_id());
  EXPECT_EQ(peers->front().id().value, peer->identifier().value());

  result_watcher_client.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);
}

TEST_F(FIDL_LowEnergyCentralServerTest, ScanReceivesPeerAddedToPeerCacheAfterScanStart) {
  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> epitaph;
  result_watcher_client.set_error_handler([&](zx_status_t cb_epitaph) { epitaph = cb_epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });

  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped);
  EXPECT_FALSE(epitaph);

  std::optional<std::vector<fble::Peer>> peers;
  result_watcher_client->Watch(
      [&](std::vector<fble::Peer> updated) { peers = std::move(updated); });
  RunLoopUntilIdle();
  ASSERT_FALSE(peers.has_value());

  bt::gap::Peer* peer = adapter()->peer_cache()->NewPeer(kTestAddr, /*connectable=*/false);
  RunLoopUntilIdle();
  ASSERT_TRUE(peers.has_value());
  ASSERT_EQ(peers->size(), 1u);
  ASSERT_TRUE(peers->front().has_id());
  EXPECT_EQ(peers->front().id().value, peer->identifier().value());

  result_watcher_client.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);
}

TEST_F(FIDL_LowEnergyCentralServerTest, ConcurrentScansFail) {
  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle_0;
  auto result_watcher_server_0 = result_watcher_handle_0.NewRequest();
  auto result_watcher_client_0 = result_watcher_handle_0.Bind();
  bool scan_stopped_0 = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server_0),
                        [&]() { scan_stopped_0 = true; });
  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped_0);

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle_1;
  auto result_watcher_server_1 = result_watcher_handle_1.NewRequest();
  auto result_watcher_client_1 = result_watcher_handle_1.Bind();
  std::optional<zx_status_t> epitaph_1;
  result_watcher_client_1.set_error_handler(
      [&](zx_status_t cb_epitaph) { epitaph_1 = cb_epitaph; });

  bool scan_stopped_1 = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server_1),
                        [&]() { scan_stopped_1 = true; });
  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped_0);
  EXPECT_TRUE(scan_stopped_1);
  ASSERT_TRUE(epitaph_1);
  EXPECT_EQ(epitaph_1.value(), ZX_ERR_ALREADY_EXISTS);

  result_watcher_client_0.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped_0);
}

TEST_F(FIDL_LowEnergyCentralServerTest, SequentialScansSucceed) {
  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle_0;
  auto result_watcher_server_0 = result_watcher_handle_0.NewRequest();
  auto result_watcher_client_0 = result_watcher_handle_0.Bind();
  bool scan_stopped_0 = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server_0),
                        [&]() { scan_stopped_0 = true; });
  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped_0);

  result_watcher_client_0.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped_0);

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle_1;
  auto result_watcher_server_1 = result_watcher_handle_1.NewRequest();
  auto result_watcher_client_1 = result_watcher_handle_1.Bind();
  bool scan_stopped_1 = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server_1),
                        [&]() { scan_stopped_1 = true; });
  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped_1);

  result_watcher_client_1.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped_1);
}

TEST_F(FIDL_LowEnergyCentralServerTest, IgnorePeersThatDoNotMatchFilter) {
  fble::ScanOptions options;
  fble::Filter filter;
  filter.set_connectable(true);
  std::vector<fble::Filter> filters;
  filters.emplace_back(std::move(filter));
  options.set_filters(std::move(filters));

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> epitaph;
  result_watcher_client.set_error_handler([&](zx_status_t cb_epitaph) { epitaph = cb_epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(std::move(options), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });

  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped);
  EXPECT_FALSE(epitaph);

  std::optional<std::vector<fble::Peer>> peers;
  result_watcher_client->Watch(
      [&](std::vector<fble::Peer> updated) { peers = std::move(updated); });
  RunLoopUntilIdle();
  ASSERT_FALSE(peers.has_value());

  // Peer is not LE
  adapter()->peer_cache()->NewPeer(
      bt::DeviceAddress(bt::DeviceAddress::Type::kBREDR, {1, 0, 0, 0, 0, 0}),
      /*connectable=*/true);
  // Peer is not connectable
  adapter()->peer_cache()->NewPeer(
      bt::DeviceAddress(bt::DeviceAddress::Type::kLEPublic, {2, 0, 0, 0, 0, 0}),
      /*connectable=*/false);

  RunLoopUntilIdle();
  EXPECT_FALSE(peers.has_value());

  result_watcher_client.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);
}

TEST_F(FIDL_LowEnergyCentralServerTest,
       DoNotNotifyResultWatcherWithPeerThatWasRemovedFromPeerCacheWhileQueued) {
  bt::gap::Peer* peer = adapter()->peer_cache()->NewPeer(kTestAddr, /*connectable=*/false);

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> epitaph;
  result_watcher_client.set_error_handler([&](zx_status_t cb_epitaph) { epitaph = cb_epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });

  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped);
  EXPECT_FALSE(epitaph);

  // Peer is in ScanResultWatcher queue. Remove it from PeerCache before Watch() is called.
  EXPECT_TRUE(adapter()->peer_cache()->RemoveDisconnectedPeer(peer->identifier()));

  std::optional<std::vector<fble::Peer>> peers;
  result_watcher_client->Watch(
      [&](std::vector<fble::Peer> updated) { peers = std::move(updated); });
  RunLoopUntilIdle();
  EXPECT_FALSE(peers.has_value());

  result_watcher_client.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);
}

TEST_F(FIDL_LowEnergyCentralServerTest, MaxQueuedScanResultWatcherPeers) {
  // Create smallest possible peer
  bt::gap::Peer* peer_0 = adapter()->peer_cache()->NewPeer(
      bt::DeviceAddress(bt::DeviceAddress::Type::kLEPublic, {0, 0, 0, 0, 0, 0}),
      /*connectable=*/false);
  const size_t kMaxPeersPerChannel = MaxPeersPerScanResultWatcherChannel(*peer_0);
  ASSERT_GT(kMaxPeersPerChannel, LowEnergyCentralServer::kMaxPendingScanResultWatcherPeers);

  // Queue 1 more peer than queue size limit.
  ASSERT_LE(LowEnergyCentralServer::kMaxPendingScanResultWatcherPeers,
            std::numeric_limits<uint8_t>::max());
  for (size_t i = 1; i < LowEnergyCentralServer::kMaxPendingScanResultWatcherPeers + 1; i++) {
    SCOPED_TRACE(i);
    ASSERT_TRUE(adapter()->peer_cache()->NewPeer(
        bt::DeviceAddress(bt::DeviceAddress::Type::kLEPublic,
                          {static_cast<uint8_t>(i), 0, 0, 0, 0, 0}),
        /*connectable=*/false));
  }

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> epitaph;
  result_watcher_client.set_error_handler([&](zx_status_t cb_epitaph) { epitaph = cb_epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });

  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped);
  EXPECT_FALSE(epitaph);

  std::optional<std::vector<fble::Peer>> peers;
  result_watcher_client->Watch(
      [&](std::vector<fble::Peer> updated) { peers = std::move(updated); });
  RunLoopUntilIdle();
  ASSERT_TRUE(peers.has_value());
  EXPECT_EQ(peers->size(), LowEnergyCentralServer::kMaxPendingScanResultWatcherPeers);
  peers.reset();

  // Additional calls to Watch should hang
  result_watcher_client->Watch(
      [&](std::vector<fble::Peer> updated) { peers = std::move(updated); });
  RunLoopUntilIdle();
  EXPECT_FALSE(peers.has_value());

  result_watcher_client.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);
}

TEST_F(FIDL_LowEnergyCentralServerTest, ScanResultWatcherMeasureTape) {
  // Create a very large Peer
  bt::gap::Peer* peer_0 = adapter()->peer_cache()->NewPeer(
      bt::DeviceAddress(bt::DeviceAddress::Type::kLEPublic, {0, 0, 0, 0, 0, 0}),
      /*connectable=*/true);
  bt::AdvertisingData adv_data;
  for (int i = 0; i < 100; i++) {
    SCOPED_TRACE(i);
    ASSERT_TRUE(adv_data.AddUri(fxl::StringPrintf("uri:a-really-long-uri-%d", i)));
  }
  adv_data.CalculateBlockSize();
  bt::DynamicByteBuffer adv_buffer(adv_data.CalculateBlockSize());
  adv_data.WriteBlock(&adv_buffer, std::nullopt);
  peer_0->MutLe().SetAdvertisingData(/*rssi=*/0, adv_buffer);

  const size_t kMaxPeersPerChannel = MaxPeersPerScanResultWatcherChannel(*peer_0);

  // Queue 1 more peer than will fit in the channel.
  // Start at i = 1 because peer_0 was created above.
  ASSERT_LE(kMaxPeersPerChannel, std::numeric_limits<uint8_t>::max());
  ASSERT_GT(LowEnergyCentralServer::kMaxPendingScanResultWatcherPeers, kMaxPeersPerChannel);
  for (size_t i = 1; i < kMaxPeersPerChannel + 1; i++) {
    SCOPED_TRACE(i);
    bt::gap::Peer* peer = adapter()->peer_cache()->NewPeer(
        bt::DeviceAddress(bt::DeviceAddress::Type::kLEPublic,
                          {static_cast<uint8_t>(i), 0, 0, 0, 0, 0}),
        /*connectable=*/false);
    ASSERT_TRUE(peer);
    peer->MutLe().SetAdvertisingData(/*rssi=*/0, adv_buffer);
  }

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> epitaph;
  result_watcher_client.set_error_handler([&](zx_status_t cb_epitaph) { epitaph = cb_epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });

  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped);
  EXPECT_FALSE(epitaph);

  std::optional<std::vector<fble::Peer>> peers;
  result_watcher_client->Watch(
      [&](std::vector<fble::Peer> updated) { peers = std::move(updated); });
  RunLoopUntilIdle();
  ASSERT_TRUE(peers.has_value());
  EXPECT_EQ(peers->size(), kMaxPeersPerChannel);
  peers.reset();

  // Additional call to Watch should return the 1 peer that exceeded the channel size limit.
  result_watcher_client->Watch(
      [&](std::vector<fble::Peer> updated) { peers = std::move(updated); });
  RunLoopUntilIdle();
  ASSERT_TRUE(peers.has_value());
  EXPECT_EQ(peers->size(), 1u);

  result_watcher_client.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);
}

TEST_F(FIDL_LowEnergyCentralServerTest, ScanResultsMatchPeerFromAnyFilter) {
  const int8_t kRssi = 0;
  // Peer that matches neither filter
  adapter()->peer_cache()->NewPeer(
      bt::DeviceAddress(bt::DeviceAddress::Type::kLEPublic, {0, 0, 0, 0, 0, 0}),
      /*connectable=*/false);

  // Peer that matches filter_0
  bt::gap::Peer* peer_0 = adapter()->peer_cache()->NewPeer(
      bt::DeviceAddress(bt::DeviceAddress::Type::kLEPublic, {1, 0, 0, 0, 0, 0}),
      /*connectable=*/true);
  ASSERT_TRUE(peer_0);
  const auto kAdvData0 = bt::StaticByteBuffer(0x02,  // Length
                                              0x09,  // AD type: Complete Local Name
                                              '0');
  peer_0->MutLe().SetAdvertisingData(kRssi, kAdvData0);
  // Peer that matches filter_1
  bt::gap::Peer* peer_1 = adapter()->peer_cache()->NewPeer(
      bt::DeviceAddress(bt::DeviceAddress::Type::kLEPublic, {2, 0, 0, 0, 0, 0}),
      /*connectable=*/false);
  ASSERT_TRUE(peer_1);
  const auto kAdvData1 = bt::StaticByteBuffer(0x02,  // Length
                                              0x09,  // AD type: Complete Local Name
                                              '1');
  peer_1->MutLe().SetAdvertisingData(kRssi, kAdvData1);

  fble::ScanOptions options;
  fble::Filter filter_0;
  filter_0.set_connectable(true);
  filter_0.set_name("0");
  fble::Filter filter_1;
  filter_1.set_connectable(false);
  filter_1.set_name("1");
  std::vector<fble::Filter> filters;
  filters.emplace_back(std::move(filter_0));
  filters.emplace_back(std::move(filter_1));
  options.set_filters(std::move(filters));

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> epitaph;
  result_watcher_client.set_error_handler([&](zx_status_t cb_epitaph) { epitaph = cb_epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(std::move(options), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });

  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped);
  EXPECT_FALSE(epitaph);

  std::optional<std::vector<bt::PeerId>> peers;
  result_watcher_client->Watch([&](std::vector<fble::Peer> updated) {
    peers = std::vector<bt::PeerId>();
    std::transform(updated.begin(), updated.end(), std::back_inserter(*peers),
                   [](auto& p) { return bt::PeerId(p.id().value); });
  });
  RunLoopUntilIdle();
  ASSERT_TRUE(peers.has_value());
  EXPECT_THAT(peers.value(), ::testing::UnorderedElementsAre(::testing::Eq(peer_0->identifier()),
                                                             ::testing::Eq(peer_1->identifier())));
  result_watcher_client.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);
}

TEST_F(FIDL_LowEnergyCentralServerTest, DiscoveryStartJustAfterScanCanceledShouldBeIgnored) {
  // Pause discovery so that we can cancel scanning before resuming discovery.
  fit::closure start_discovery;
  test_device()->pause_responses_for_opcode(
      bt::hci::kLESetScanEnable,
      [&](auto resume_set_scan_enable) { start_discovery = std::move(resume_set_scan_enable); });

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();

  bool scan_stopped = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });

  RunLoopUntilIdle();
  EXPECT_FALSE(scan_stopped);
  EXPECT_TRUE(start_discovery);

  result_watcher_client.Unbind();
  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);

  start_discovery();
  RunLoopUntilIdle();
}

TEST_F(FIDL_LowEnergyCentralServerTest, ScanFailsToStart) {
  test_device()->SetDefaultResponseStatus(bt::hci::kLESetScanEnable,
                                          bt::hci::StatusCode::kControllerBusy);

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> epitaph;
  result_watcher_client.set_error_handler([&](zx_status_t cb_epitaph) { epitaph = cb_epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });

  RunLoopUntilIdle();
  EXPECT_TRUE(scan_stopped);
  ASSERT_TRUE(epitaph);
  EXPECT_EQ(*epitaph, ZX_ERR_INTERNAL);
}

TEST_F(FIDL_LowEnergyCentralServerTest, ScanSessionErrorCancelsScan) {
  zx::duration kTestScanPeriod = zx::sec(1);
  adapter()->le()->set_scan_period_for_testing(kTestScanPeriod);
  std::vector<bool> scan_states;
  test_device()->set_scan_state_callback([&](bool enabled) {
    scan_states.push_back(enabled);
    // Wait for 2 state transitions: -> enabled -> disabled.
    // Then disable restarting scanning, so that an error is sent to sessions.
    if (scan_states.size() == 2u) {
      EXPECT_FALSE(enabled);
      test_device()->SetDefaultResponseStatus(bt::hci::kLESetScanEnable,
                                              bt::hci::StatusCode::kCommandDisallowed);
    }
  });

  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> epitaph;
  result_watcher_client.set_error_handler([&](zx_status_t cb_epitaph) { epitaph = cb_epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });
  RunLoopFor(kTestScanPeriod);
  EXPECT_TRUE(scan_stopped);
  ASSERT_TRUE(epitaph);
  EXPECT_EQ(*epitaph, ZX_ERR_INTERNAL);
}

TEST_F(FIDL_LowEnergyCentralServerTest,
       ScanResultWatcherWatchCalledBeforePreviousWatchReceivedResponse) {
  fidl::InterfaceHandle<fble::ScanResultWatcher> result_watcher_handle;
  auto result_watcher_server = result_watcher_handle.NewRequest();
  auto result_watcher_client = result_watcher_handle.Bind();
  std::optional<zx_status_t> epitaph;
  result_watcher_client.set_error_handler([&](zx_status_t cb_epitaph) { epitaph = cb_epitaph; });

  bool scan_stopped = false;
  central_proxy()->Scan(ScanOptionsWithEmptyFilter(), std::move(result_watcher_server),
                        [&]() { scan_stopped = true; });
  bool watch_response_0 = false;
  result_watcher_client->Watch([&](auto) { watch_response_0 = true; });
  bool watch_response_1 = false;
  result_watcher_client->Watch([&](auto) { watch_response_1 = true; });
  RunLoopUntilIdle();
  EXPECT_FALSE(watch_response_0);
  EXPECT_FALSE(watch_response_1);
  EXPECT_TRUE(scan_stopped);
  ASSERT_TRUE(epitaph);
  EXPECT_EQ(*epitaph, ZX_ERR_CANCELED);
}

}  // namespace
}  // namespace bthost
