// Thin adapter around the pinned MediaPipe 1.1.0 C API. Inference is worker-owned.
#define NOMINMAX
#define MP_EXPORT
#include <windows.h>
#include <algorithm>
#include <cmath>
#include <cstdio>
#include <filesystem>
#include <fstream>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>
#include "mediapipe/tasks/c/vision/face_landmarker/face_landmarker.h"
#include "mediapipe/tasks/c/vision/pose_landmarker/pose_landmarker.h"
#include "mediapipe/tasks/c/vision/image_segmenter/image_segmenter.h"

#define AI_FUNCTIONS(X) \
 X(MpErrorFree) X(MpImageCreateFromUint8Data) X(MpImageFree) \
 X(MpImageGetWidth) X(MpImageGetHeight) X(MpImageDataFloat32) \
 X(MpFaceLandmarkerCreate) X(MpFaceLandmarkerDetectForVideo) \
 X(MpFaceLandmarkerCloseResult) X(MpFaceLandmarkerClose) \
 X(MpPoseLandmarkerCreate) X(MpPoseLandmarkerDetectForVideo) \
 X(MpPoseLandmarkerCloseResult) X(MpPoseLandmarkerClose) \
 X(MpImageSegmenterCreate) X(MpImageSegmenterSegmentForVideo) \
 X(MpImageSegmenterCloseResult) X(MpImageSegmenterClose)

struct AiResult {
    float face[478][4]; // x, y, z, confidence
    float pose[33][4];
    uint8_t mask[256 * 256 * 2]; // skin probability, person probability
    uint32_t face_count, pose_count;
};
struct Engine {
    HMODULE dll = nullptr;
#define DECLARE(name) decltype(&name) name = nullptr;
    AI_FUNCTIONS(DECLARE)
#undef DECLARE
    MpFaceLandmarkerPtr face = nullptr;
    MpPoseLandmarkerPtr pose = nullptr;
    MpImageSegmenterPtr skin = nullptr;
    std::vector<char> models[3];
    ~Engine() {
        if (face) MpFaceLandmarkerClose(face, nullptr);
        if (pose) MpPoseLandmarkerClose(pose, nullptr);
        if (skin) MpImageSegmenterClose(skin, nullptr);
        if (dll) FreeLibrary(dll);
    }
    void check(MpStatus status, char* error) {
        std::string message = error ? error : "MediaPipe operation failed";
        if (error) MpErrorFree(error);
        if (status != kMpOk) throw std::runtime_error(message);
    }
    MpBaseOptions options(const std::filesystem::path& path, int index) {
        std::ifstream file(path, std::ios::binary);
        if (!file) throw std::runtime_error("Missing AI model; run scripts/setup-ai.ps1");
        models[index] = std::vector<char>(std::istreambuf_iterator<char>(file), {});
        MpBaseOptions out{};
        out.model_asset_buffer = models[index].data();
        out.model_asset_buffer_count = static_cast<unsigned>(models[index].size());
        out.file_descriptor = -1;
        out.host_system = MP_HOST_SYSTEM_WINDOWS;
        return out;
    }
};

extern "C" void* nkg_ai_open(const wchar_t* directory, unsigned flags, char* error, int capacity) {
    try {
        auto e = std::make_unique<Engine>();
        const std::filesystem::path root(directory);
        e->dll = LoadLibraryExW((root / L"mediapipe.dll").c_str(), nullptr,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
        if (!e->dll) throw std::runtime_error("Cannot load mediapipe.dll; run scripts/setup-ai.ps1 (VC++ runtime required)");
#define LOAD(name) e->name = reinterpret_cast<decltype(e->name)>(GetProcAddress(e->dll, #name)); \
        if (!e->name) throw std::runtime_error("MediaPipe 1.1.0 symbol missing: " #name);
        AI_FUNCTIONS(LOAD)
#undef LOAD
        char* message = nullptr;
        if (flags & 1) {
            MpFaceLandmarkerOptions opts{};
            opts.base_options = e->options(root / L"models/face_landmarker.task", 0);
            opts.running_mode = MP_RUNNING_MODE_VIDEO;
            auto status = e->MpFaceLandmarkerCreate(&opts, &e->face, &message);
            e->check(status, message); message = nullptr;
        }
        if (flags & 2) {
            MpPoseLandmarkerOptions opts{};
            opts.base_options = e->options(root / L"models/pose_landmarker.task", 1);
            opts.running_mode = MP_RUNNING_MODE_VIDEO;
            auto status = e->MpPoseLandmarkerCreate(&opts, &e->pose, &message);
            e->check(status, message); message = nullptr;
        }
        if (flags & 4) {
            MpImageSegmenterOptions opts{};
            opts.display_names_locale = "en";
            opts.base_options = e->options(root / L"models/selfie_multiclass.tflite", 2);
            opts.running_mode = MP_RUNNING_MODE_VIDEO;
            auto status = e->MpImageSegmenterCreate(&opts, &e->skin, &message);
            e->check(status, message);
        }
        return e.release();
    } catch (const std::exception& ex) {
        snprintf(error, capacity, "%s", ex.what());
        return nullptr;
    }
}

extern "C" int nkg_ai_run(void* handle, const uint8_t* rgba, int width, int height,
    int64_t timestamp, AiResult* output, char* error, int capacity) {
    auto& e = *static_cast<Engine*>(handle);
    MpImagePtr image = nullptr;
    MpFaceLandmarkerResult face{};
    MpPoseLandmarkerResult pose{};
    MpImageSegmenterResult skin{};
    int result = 0;
    try {
        if (!rgba || width < 1 || height < 1 || width > 1024 || height > 1024)
            throw std::runtime_error("Invalid inference image dimensions");
        *output = AiResult{};
        char* message = nullptr;
#define CHECK(call) { auto status = (call); e.check(status, message); message = nullptr; }
        CHECK(e.MpImageCreateFromUint8Data(kMpImageFormatSrgba, width, height,
            rgba, width * height * 4, &image, &message));
        auto copy = [](const MpNormalizedLandmarks& source, float (*dest)[4], unsigned count) {
            if (source.landmarks_count < count) throw std::runtime_error("Unexpected landmark model output");
            for (unsigned i = 0; i < count; ++i) {
                const auto& p = source.landmarks[i];
                dest[i][0] = p.x; dest[i][1] = p.y; dest[i][2] = p.z;
                dest[i][3] = std::min(p.has_visibility ? p.visibility : 1.f,
                    p.has_presence ? p.presence : 1.f);
            }
        };
        if (e.face) {
            CHECK(e.MpFaceLandmarkerDetectForVideo(e.face, image, nullptr, timestamp, &face, &message));
            if (face.face_landmarks_count) {
                copy(face.face_landmarks[0], output->face, 478);
                output->face_count = 478;
            }
        }
        if (e.pose) {
            CHECK(e.MpPoseLandmarkerDetectForVideo(e.pose, image, nullptr, timestamp, &pose, &message));
            if (pose.pose_landmarks_count) {
                copy(pose.pose_landmarks[0], output->pose, 33);
                output->pose_count = 33;
            }
        }
        if (e.skin) {
            CHECK(e.MpImageSegmenterSegmentForVideo(e.skin, image, nullptr, timestamp, &skin, &message));
            if (skin.confidence_masks_count != 6) throw std::runtime_error("Expected six skin segmentation classes");
            const float *background, *body, *face_skin;
            CHECK(e.MpImageDataFloat32(skin.confidence_masks[0], &background, &message));
            CHECK(e.MpImageDataFloat32(skin.confidence_masks[2], &body, &message));
            CHECK(e.MpImageDataFloat32(skin.confidence_masks[3], &face_skin, &message));
            int w = e.MpImageGetWidth(skin.confidence_masks[0]);
            int h = e.MpImageGetHeight(skin.confidence_masks[0]);
            if (w < 1 || h < 1) throw std::runtime_error("Empty segmentation mask");
            for (int y = 0; y < 256; ++y) for (int x = 0; x < 256; ++x) {
                int p = std::min(h - 1, y * h / 256) * w + std::min(w - 1, x * w / 256);
                output->mask[(y * 256 + x) * 2] = static_cast<uint8_t>(255 * std::clamp(body[p] + face_skin[p], 0.f, 1.f));
                output->mask[(y * 256 + x) * 2 + 1] = static_cast<uint8_t>(255 * std::clamp(1.f - background[p], 0.f, 1.f));
            }
        }
#undef CHECK
    } catch (const std::exception& ex) {
        snprintf(error, capacity, "%s", ex.what()); result = -1;
    }
    e.MpFaceLandmarkerCloseResult(&face);
    e.MpPoseLandmarkerCloseResult(&pose);
    e.MpImageSegmenterCloseResult(&skin);
    if (image) e.MpImageFree(image);
    return result;
}
extern "C" void nkg_ai_close(void* handle) { delete static_cast<Engine*>(handle); }
