// A call requested while a gRPC server shuts down can be lost: its tag never
// comes back, so its completion queue never finishes shutting down.
//
// Server::QueueRequestedCall checks ShutdownCalled() and then queues the
// request; Server::ShutdownAndNotify fails queued requests, marks shutdown as
// called, and (with no listeners or channels left) fails them once more and
// publishes the shutdown. A request queued after that last kill is never
// failed. Found with armem (https://github.com/russellw/armem), whose
// c/grpc_server.cpp models the race.
//
// Each iteration starts a server with no ports, then one thread calls
// grpc_server_request_call while another calls grpc_server_shutdown_and_notify.
// Both tags should come back: the shutdown's, and the request's with
// success=0. If the request's doesn't, the request was lost; the program
// then shows that grpc_completion_queue_shutdown never completes, and exits 1.
//
// usage: repro [ITERATIONS] [PAIRS]
//   PAIRS servers race at once, on as many thread pairs (default 1).
// With a gRPC built with the delay hook in QueueRequestedCall, set
// GRPC_REPRO_QUEUE_DELAY_US (microseconds) to widen the window and lose the
// request every time.

#include <grpc/grpc.h>

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <thread>
#include <vector>

namespace {

void* const kRequestTag = reinterpret_cast<void*>(1);
void* const kShutdownTag = reinterpret_cast<void*>(2);

gpr_timespec In(int ms) {
  return gpr_time_add(gpr_now(GPR_CLOCK_MONOTONIC),
                      gpr_time_from_millis(ms, GPR_TIMESPAN));
}

std::atomic<bool> lost{false};

// One race; returns false if the request was lost.
bool RaceOnce(unsigned seed) {
  grpc_completion_queue* cq = grpc_completion_queue_create_for_next(nullptr);
  grpc_server* server = grpc_server_create(nullptr, nullptr);
  grpc_server_register_completion_queue(server, cq, nullptr);
  grpc_server_start(server);

  grpc_call* call = nullptr;
  grpc_call_details details;
  grpc_metadata_array metadata;
  grpc_call_details_init(&details);
  grpc_metadata_array_init(&metadata);

  // Stagger the two threads by a varying number of spins, so that over many
  // iterations the request's check and push fall at every point of the
  // shutdown.
  std::atomic<int> go{0};
  int spin_req = seed % 64, spin_shut = (seed / 64) % 64;
  std::thread requester([&] {
    while (go.load(std::memory_order_acquire) == 0) {
    }
    for (volatile int i = 0; i < spin_req; i++) {
    }
    grpc_call_error e = grpc_server_request_call(server, &call, &details,
                                                 &metadata, cq, cq, kRequestTag);
    if (e != GRPC_CALL_OK) {
      fprintf(stderr, "grpc_server_request_call: %d\n", e);
      abort();
    }
  });
  std::thread shutter([&] {
    while (go.load(std::memory_order_acquire) == 0) {
    }
    for (volatile int i = 0; i < spin_shut; i++) {
    }
    // With the delay hook, start the shutdown halfway through the
    // requester's delay, after its check and before its push.
    if (const char* delay = getenv("GRPC_REPRO_QUEUE_DELAY_US")) {
      std::this_thread::sleep_for(std::chrono::microseconds(atoi(delay) / 2));
    }
    grpc_server_shutdown_and_notify(server, cq, kShutdownTag);
  });
  go.store(1, std::memory_order_release);
  requester.join();
  shutter.join();

  bool got_request = false, got_shutdown = false;
  while (!(got_request && got_shutdown)) {
    grpc_event ev = grpc_completion_queue_next(cq, In(5000), nullptr);
    if (ev.type == GRPC_QUEUE_TIMEOUT) break;
    if (ev.type != GRPC_OP_COMPLETE) abort();
    if (ev.tag == kRequestTag) {
      if (ev.success) {
        fprintf(stderr, "request succeeded with no client\n");
        abort();
      }
      got_request = true;
    } else if (ev.tag == kShutdownTag) {
      got_shutdown = true;
    }
  }
  if (!got_request) {
    lost = true;
    printf("LOST: the requested call's tag never came back (shutdown tag %s)\n",
           got_shutdown ? "did" : "did not either");
    // The request still holds an operation on cq, so cq can't finish
    // shutting down. (Destroying the server instead fails
    // GRPC_CHECK_EQ(queue.Pop(), nullptr) in ~RealRequestMatcher.)
    grpc_completion_queue_shutdown(cq);
    grpc_event ev = grpc_completion_queue_next(cq, In(5000), nullptr);
    printf("after grpc_completion_queue_shutdown, next() gives %s after 5 s\n",
           ev.type == GRPC_QUEUE_SHUTDOWN ? "SHUTDOWN"
           : ev.type == GRPC_QUEUE_TIMEOUT ? "TIMEOUT (the queue never shuts down)"
                                           : "an event");
    return false;
  }

  grpc_server_destroy(server);
  grpc_completion_queue_shutdown(cq);
  while (grpc_completion_queue_next(cq, In(5000), nullptr).type !=
         GRPC_QUEUE_SHUTDOWN) {
  }
  grpc_completion_queue_destroy(cq);
  grpc_call_details_destroy(&details);
  grpc_metadata_array_destroy(&metadata);
  return true;
}

}  // namespace

int main(int argc, char** argv) {
  long iterations = argc > 1 ? atol(argv[1]) : 100000;
  int pairs = argc > 2 ? atoi(argv[2]) : 1;
  grpc_init();
  std::atomic<long> done{0};
  std::vector<std::thread> workers;
  for (int p = 0; p < pairs; p++) {
    workers.emplace_back([&, p] {
      for (long i = p; i < iterations && !lost; i += pairs) {
        if (!RaceOnce(static_cast<unsigned>(i * 2654435761u >> 8))) return;
        long n = ++done;
        if (n % 10000 == 0) printf("%ld iterations, none lost\n", n);
      }
    });
  }
  for (auto& w : workers) w.join();
  if (lost) {
    // Skip grpc_shutdown: the lost request's server and queue can't be
    // cleaned up.
    fflush(stdout);
    _Exit(1);
  }
  printf("%ld iterations, none lost\n", done.load());
  grpc_shutdown();
  return 0;
}
