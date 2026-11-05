#pragma once

#include <atomic>
#include <algorithm>
#include <condition_variable>
#include <chrono>
#include <cerrno>
#include <cstdint>
#include <cstdlib>
#include <deque>
#include <iostream>
#include <mutex>
#include <set>
#include <string>
#include <thread>
#include <vector>
#include <unistd.h>
#include "json.hpp"

namespace voice {
using json = nlohmann::json;
constexpr uint32_t max_frame = 4U * 1024U * 1024U;
struct Failure { std::string code; bool retryable = false; };
struct Command { json data; std::vector<uint8_t> pcm; bool binary = false; };

class Runtime {
    std::mutex output_mutex;
    std::mutex queue_mutex;
    std::condition_variable available;
    std::deque<Command> queue;
    std::set<std::string> pending_cancels;
    std::string current_job;
    std::string worker_kind;

    static bool read_exact(void * buffer, size_t size) {
        auto * bytes = static_cast<char *>(buffer);
        while (size) {
            const auto count = ::read(STDIN_FILENO, bytes, size);
            if (count < 0 && errno == EINTR) continue;
            if (count <= 0) return false;
            bytes += count;
            size -= static_cast<size_t>(count);
        }
        return true;
    }

    void reader() {
        for (;;) {
            uint8_t header[4];
            if (!read_exact(header, sizeof(header))) std::_Exit(0);
            const uint32_t length = uint32_t(header[0]) | (uint32_t(header[1]) << 8U)
                | (uint32_t(header[2]) << 16U) | (uint32_t(header[3]) << 24U);
            if (length < 1 || length > max_frame) {
                error("invalid_frame", "", false);
                std::_Exit(2);
            }
            std::vector<uint8_t> payload(length);
            if (!read_exact(payload.data(), payload.size())) std::_Exit(0);
            Command command;
            try {
                if (payload[0] == 1) {
                    if (payload.size() > 65537) throw Failure{"control_too_large"};
                    command.data = json::parse(payload.begin() + 1, payload.end());
                    if (!command.data.is_object() || !command.data.contains("op")
                        || !command.data["op"].is_string()) throw Failure{"invalid_request"};
                    const auto op = command.data["op"].get<std::string>();
                    std::set<std::string> allowed{"op"};
                    if (op == "load") allowed.insert({"model_path", "threads", "context_tokens", "gpu_layers"});
                    else if (op == "run") {
                        allowed.insert("job_id");
                        if (worker_kind == "asr") allowed.insert("language");
                        else allowed.insert({"messages", "max_tokens"});
                    } else if (op == "cancel") allowed.insert("job_id");
                    for (const auto & item : command.data.items()) {
                        if (!allowed.count(item.key())) throw Failure{"unknown_field"};
                    }
                    if (op == "cancel") {
                        const auto job = command.data.value("job_id", std::string{});
                        if (job.empty() || job.size() > 128) throw Failure{"invalid_job_id"};
                        std::lock_guard<std::mutex> lock(queue_mutex);
                        if (current_job == job) {
                            if (!cancelled.exchange(true)) {
                                // Some upstream CPU graphs only poll their abort callback
                                // between long encoder passes. Bound cancellation without
                                // touching the pinned engine or leaking inference threads.
                                std::thread([this, job] {
                                    std::this_thread::sleep_for(std::chrono::milliseconds(350));
                                    std::lock_guard<std::mutex> guard(queue_mutex);
                                    if (current_job == job && cancelled.load()) {
                                        current_job.clear();
                                        send({{"event", "cancelled"}, {"job_id", job}});
                                        std::_Exit(0);
                                    }
                                }).detach();
                            }
                        } else {
                            bool queued = false;
                            for (const auto & item : queue) {
                                if (!item.binary && item.data.value("op", "") == "run"
                                    && item.data.value("job_id", "") == job) queued = true;
                            }
                            if (queued) pending_cancels.insert(job);
                            else send({{"event", "cancelled"}, {"job_id", job}});
                        }
                        continue;
                    }
                    if (op != "load" && op != "run" && op != "unload" && op != "shutdown")
                        throw Failure{"unknown_operation"};
                    if (op == "run") {
                        const auto job = command.data.value("job_id", std::string{});
                        if (job.empty() || job.size() > 128) throw Failure{"invalid_job_id"};
                    }
                    if (op == "shutdown") cancelled.store(true);
                } else if (payload[0] == 2) {
                    command.binary = true;
                    command.pcm.assign(payload.begin() + 1, payload.end());
                } else {
                    throw Failure{"unknown_frame_type"};
                }
                {
                    std::lock_guard<std::mutex> lock(queue_mutex);
                    if (queue.size() >= 4) throw Failure{"queue_overflow"};
                    queue.push_back(std::move(command));
                }
                available.notify_one();
            } catch (const Failure & failure) {
                error(failure.code, "", failure.retryable);
                std::_Exit(2);
            } catch (...) {
                error("invalid_request", "", false);
                std::_Exit(2);
            }
        }
    }

public:
    std::atomic<bool> cancelled{false};

    void start(const char * worker) {
        worker_kind = worker;
        send({{"event", "hello"}, {"protocol", 1}, {"worker", worker}});
        std::thread([this] { reader(); }).detach();
    }

    void send(const json & response) {
        std::lock_guard<std::mutex> lock(output_mutex);
        const std::string encoded = response.dump();
        if (encoded.size() + 1 > max_frame) std::_Exit(2);
        const auto length = static_cast<uint32_t>(encoded.size() + 1);
        const uint8_t header[] = {uint8_t(length), uint8_t(length >> 8U), uint8_t(length >> 16U), uint8_t(length >> 24U), 1};
        std::cout.write(reinterpret_cast<const char *>(header), sizeof(header));
        std::cout.write(encoded.data(), static_cast<std::streamsize>(encoded.size()));
        std::cout.flush();
        if (!std::cout) std::_Exit(2);
    }

    void error(const std::string & code, const std::string & job, bool retryable) {
        json response = {{"event", "error"}, {"code", code}, {"retryable", retryable}};
        if (!job.empty()) response["job_id"] = job;
        send(response);
    }

    Command next() {
        std::unique_lock<std::mutex> lock(queue_mutex);
        available.wait(lock, [this] { return !queue.empty(); });
        auto command = std::move(queue.front());
        queue.pop_front();
        if (!command.binary && command.data.value("op", "") == "run") {
            current_job = command.data.at("job_id").get<std::string>();
            cancelled.store(pending_cancels.erase(current_job) != 0);
        }
        return command;
    }

    void finish_job() {
        std::lock_guard<std::mutex> lock(queue_mutex);
        current_job.clear();
    }

    void acknowledge_cancelled(const std::string & job) {
        std::lock_guard<std::mutex> lock(queue_mutex);
        if (current_job == job) {
            current_job.clear();
            send({{"event", "cancelled"}, {"job_id", job}});
        }
    }

    static bool should_abort(void * state) {
        return static_cast<Runtime *>(state)->cancelled.load();
    }
};

inline int threads_from(const json & request) {
    const int available = static_cast<int>(std::max(1U, std::thread::hardware_concurrency()));
    const int threads = request.value("threads", 0);
    if (threads < 0 || threads > available) throw Failure{"invalid_threads"};
    return threads == 0 ? std::min(4, available) : threads;
}

inline std::string model_path_from(const json & request) {
    const auto path = request.at("model_path").get<std::string>();
    if (path.empty() || path.size() > 4096 || path.find('\0') != std::string::npos)
        throw Failure{"invalid_model_path"};
    return path;
}
} // namespace voice
