#include <d3d11_4.h>
#include <d3d12.h>
#include <dxgi1_4.h>
#include <wrl/client.h>
#include <memory>
#include <cstdio>
#include <vector>
#include <mutex>
#include <atomic>
extern "C" {
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>
#include <libavutil/frame.h>
}
using Microsoft::WRL::ComPtr;
struct GpuSurface {
    ComPtr<ID3D12Resource> resource;
    ComPtr<ID3D11Texture2D> texture;
    ComPtr<ID3D11VideoProcessorOutputView> view;
    ComPtr<ID3D12Fence> fence;
    UINT64 value=0;
};
struct GpuPool {
    std::mutex mutex;
    std::vector<std::unique_ptr<GpuSurface>> idle;
    size_t capacity=0;
    std::atomic<UINT64> allocations{0};
};
struct GpuFrame {
    std::unique_ptr<GpuSurface> surface;
    std::shared_ptr<GpuPool> pool;
};
struct GpuDecoder {
    ComPtr<ID3D12Device> output;
    ComPtr<ID3D11Device> device;
    ComPtr<ID3D11DeviceContext> context;
    ComPtr<ID3D11DeviceContext4> context4;
    ComPtr<ID3D11VideoDevice> video;
    ComPtr<ID3D11VideoContext1> video_context;
    ComPtr<ID3D11VideoProcessorEnumerator> enumerator;
    ComPtr<ID3D11VideoProcessor> processor;
    ComPtr<ID3D11Texture2D> input_texture;
    std::vector<ComPtr<ID3D11VideoProcessorInputView>> input_views;
    ComPtr<ID3D12Fence> fence12;
    ComPtr<ID3D11Fence> fence11;
    UINT64 serial=0;
    int width=0,height=0,rotation=-1;
    DXGI_COLOR_SPACE_TYPE color=DXGI_COLOR_SPACE_CUSTOM;
    std::shared_ptr<GpuPool> pool;
};
static int error(HRESULT hr, const char *where, char *text, int len) {
    snprintf(text,len,"%s: HRESULT 0x%08lx",where,(unsigned long)hr); return -1;
}
#define CHECK(call) do { HRESULT checked_result=(call); if(FAILED(checked_result)) return error(checked_result,#call,message,length); } while(0)
extern "C" int nkg_gpu_open(void *raw, void **result, AVBufferRef **hardware, char *message, int length) {
    auto gpu=std::make_unique<GpuDecoder>();
    gpu->output=static_cast<ID3D12Device *>(raw);
    ComPtr<IDXGIFactory4> factory;
    CHECK(CreateDXGIFactory1(IID_PPV_ARGS(&factory)));
    ComPtr<IDXGIAdapter> adapter;
    CHECK(factory->EnumAdapterByLuid(gpu->output->GetAdapterLuid(),IID_PPV_ARGS(&adapter)));
    CHECK(D3D11CreateDevice(adapter.Get(),D3D_DRIVER_TYPE_UNKNOWN,nullptr,
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT|D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        nullptr,0,D3D11_SDK_VERSION,&gpu->device,nullptr,&gpu->context));
    CHECK(gpu->context.As(&gpu->context4));
    CHECK(gpu->device.As(&gpu->video));
    CHECK(gpu->context.As(&gpu->video_context));
    ComPtr<ID3D11Device5> device5;
    CHECK(gpu->device.As(&device5));
    CHECK(gpu->output->CreateFence(0,D3D12_FENCE_FLAG_SHARED,IID_PPV_ARGS(&gpu->fence12)));
    HANDLE handle=nullptr;
    CHECK(gpu->output->CreateSharedHandle(gpu->fence12.Get(),nullptr,GENERIC_ALL,nullptr,&handle));
    HRESULT hr=device5->OpenSharedFence(handle,IID_PPV_ARGS(&gpu->fence11));
    CloseHandle(handle);
    CHECK(hr);
    AVBufferRef *hw=av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
    if (!hw) return error(E_OUTOFMEMORY,"hardware context",message,length);
    auto native=static_cast<AVD3D11VADeviceContext *>(reinterpret_cast<AVHWDeviceContext *>(hw->data)->hwctx);
    native->device=gpu->device.Get(); native->device->AddRef();
    int code=av_hwdevice_ctx_init(hw);
    if (code<0) { av_buffer_unref(&hw); snprintf(message,length,"FFmpeg D3D11 device init: %d",code); return -1; }
    *hardware=hw; *result=gpu.release(); return 0;
}
extern "C" void nkg_gpu_close(void *p) { delete static_cast<GpuDecoder *>(p); }
extern "C" void nkg_gpu_frame_close(void *p) { delete static_cast<GpuFrame *>(p); }
extern "C" UINT64 nkg_gpu_frame_allocations(void *p) {
    return static_cast<GpuFrame *>(p)->pool->allocations.load(std::memory_order_relaxed);
}
// Called only after Rust owners and submitted rendering have finished with the frame.
extern "C" void nkg_gpu_frame_recycle(void *p) {
    std::unique_ptr<GpuFrame> frame(static_cast<GpuFrame *>(p));
    std::lock_guard<std::mutex> lock(frame->pool->mutex);
    if (frame->pool->idle.size()<frame->pool->capacity)
        frame->pool->idle.push_back(std::move(frame->surface));
}
extern "C" void *nkg_gpu_resource(void *p) {
    auto resource=static_cast<GpuFrame *>(p)->surface->resource.Get(); resource->AddRef(); return resource;
}
extern "C" int nkg_gpu_wait(void *p, void *queue) {
    auto surface=static_cast<GpuFrame *>(p)->surface.get();
    return FAILED(static_cast<ID3D12CommandQueue *>(queue)->Wait(surface->fence.Get(),surface->value)) ? -1 : 0;
}
extern "C" int nkg_gpu_convert(void *p, AVFrame *source, int rotation, void **result, char *message, int length) {
    auto gpu=static_cast<GpuDecoder *>(p);
    const int w=source->width,h=source->height;
    if (!gpu->processor || gpu->width!=w || gpu->height!=h || gpu->rotation!=rotation) {
        gpu->input_views.clear(); gpu->input_texture.Reset();
        gpu->processor.Reset(); gpu->enumerator.Reset();
        D3D11_VIDEO_PROCESSOR_CONTENT_DESC desc={};
        desc.InputFrameFormat=D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
        desc.InputWidth=w; desc.InputHeight=h;
        desc.OutputWidth=(rotation==90||rotation==270)?h:w;
        desc.OutputHeight=(rotation==90||rotation==270)?w:h;
        desc.Usage=D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;
        CHECK(gpu->video->CreateVideoProcessorEnumerator(&desc,&gpu->enumerator));
        CHECK(gpu->video->CreateVideoProcessor(gpu->enumerator.Get(),0,&gpu->processor));
        gpu->pool=std::make_shared<GpuPool>();
        // ponytail: retain at most 3 idle surfaces / 64 MiB (before driver alignment).
        // Busy surfaces stay with frames; tune only if measured allocation churn remains.
        gpu->pool->capacity=(64ull*1024*1024)/(static_cast<UINT64>(w)*h*4);
        if (gpu->pool->capacity>3) gpu->pool->capacity=3;
        gpu->pool->idle.reserve(gpu->pool->capacity);
        gpu->width=w; gpu->height=h; gpu->rotation=rotation;
        gpu->color=DXGI_COLOR_SPACE_CUSTOM;
        gpu->video_context->VideoProcessorSetOutputColorSpace1(gpu->processor.Get(),DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
        gpu->video_context->VideoProcessorSetStreamAutoProcessingMode(gpu->processor.Get(),0,FALSE);
        gpu->video_context->VideoProcessorSetStreamFrameFormat(gpu->processor.Get(),0,D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
        RECT src={0,0,w,h}, dst={0,0,static_cast<LONG>(desc.OutputWidth),static_cast<LONG>(desc.OutputHeight)};
        gpu->video_context->VideoProcessorSetStreamSourceRect(gpu->processor.Get(),0,TRUE,&src);
        gpu->video_context->VideoProcessorSetStreamDestRect(gpu->processor.Get(),0,TRUE,&dst);
        gpu->video_context->VideoProcessorSetOutputTargetRect(gpu->processor.Get(),TRUE,&dst);
        gpu->video_context->VideoProcessorSetStreamRotation(gpu->processor.Get(),0,rotation!=0,
            rotation==90?D3D11_VIDEO_PROCESSOR_ROTATION_270:rotation==270?D3D11_VIDEO_PROCESSOR_ROTATION_90:rotation==180?D3D11_VIDEO_PROCESSOR_ROTATION_180:D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY);
    }
    auto frame=std::make_unique<GpuFrame>();
    frame->pool=gpu->pool;
    {
        std::lock_guard<std::mutex> lock(gpu->pool->mutex);
        for (auto it=gpu->pool->idle.begin();it!=gpu->pool->idle.end();++it) {
            const auto completed=(*it)->fence->GetCompletedValue();
            // Even undisplayed frames may still be written by D3D11. Never wait on CPU.
            if (completed!=UINT64_MAX && completed>=(*it)->value) {
                frame->surface=std::move(*it); gpu->pool->idle.erase(it); break;
            }
        }
    }
    if (!frame->surface) {
        frame->surface=std::make_unique<GpuSurface>();
        auto &surface=*frame->surface;
        D3D12_HEAP_PROPERTIES heap={}; heap.Type=D3D12_HEAP_TYPE_DEFAULT;
        D3D12_RESOURCE_DESC desc={}; desc.Dimension=D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        desc.Width=(rotation==90||rotation==270)?h:w;
        desc.Height=(rotation==90||rotation==270)?w:h; desc.DepthOrArraySize=1; desc.MipLevels=1;
        desc.Format=DXGI_FORMAT_B8G8R8A8_UNORM; desc.SampleDesc.Count=1;
        desc.Flags=D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET|D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS;
        CHECK(gpu->output->CreateCommittedResource(&heap,D3D12_HEAP_FLAG_SHARED,&desc,
            D3D12_RESOURCE_STATE_COMMON,nullptr,IID_PPV_ARGS(&surface.resource)));
        gpu->pool->allocations.fetch_add(1,std::memory_order_relaxed);
        HANDLE handle=nullptr;
        CHECK(gpu->output->CreateSharedHandle(surface.resource.Get(),nullptr,GENERIC_ALL,nullptr,&handle));
        ComPtr<ID3D11Device1> device1; HRESULT hr=gpu->device.As(&device1);
        if (SUCCEEDED(hr)) hr=device1->OpenSharedResource1(handle,IID_PPV_ARGS(&surface.texture));
        CloseHandle(handle); CHECK(hr);
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC output_desc={};
        output_desc.ViewDimension=D3D11_VPOV_DIMENSION_TEXTURE2D;
        CHECK(gpu->video->CreateVideoProcessorOutputView(surface.texture.Get(),gpu->enumerator.Get(),&output_desc,&surface.view));
        surface.fence=gpu->fence12;
    }
    auto input=reinterpret_cast<ID3D11Texture2D *>(source->data[0]);
    const auto slice=reinterpret_cast<uintptr_t>(source->data[1]);
    if (!input) return error(E_INVALIDARG,"input texture",message,length);
    if (gpu->input_texture.Get()!=input) {
        D3D11_TEXTURE2D_DESC input_desc={}; input->GetDesc(&input_desc);
        gpu->input_views.clear();
        gpu->input_views.resize(input_desc.ArraySize);
        // Views retain the texture, not AVFrames: decoder slices remain reusable.
        // Cache only the current array so replacement pools cannot accumulate.
        gpu->input_texture=input;
    }
    if (slice>=gpu->input_views.size()) return error(E_INVALIDARG,"input array slice",message,length);
    auto &input_view=gpu->input_views[slice];
    if (!input_view) {
        D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC input_desc={};
        input_desc.ViewDimension=D3D11_VPIV_DIMENSION_TEXTURE2D;
        input_desc.Texture2D.ArraySlice=static_cast<UINT>(slice);
        CHECK(gpu->video->CreateVideoProcessorInputView(input,gpu->enumerator.Get(),&input_desc,&input_view));
    }
    const bool full=source->color_range==AVCOL_RANGE_JPEG, bt709=source->colorspace==AVCOL_SPC_BT709;
    DXGI_COLOR_SPACE_TYPE color=bt709 ? (full?DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709:DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709)
        : (full?DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601:DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601);
    if (gpu->color!=color) {
        gpu->video_context->VideoProcessorSetStreamColorSpace1(gpu->processor.Get(),0,color);
        gpu->color=color;
    }
    D3D11_VIDEO_PROCESSOR_STREAM stream={}; stream.Enable=TRUE; stream.pInputSurface=input_view.Get();
    CHECK(gpu->video_context->VideoProcessorBlt(gpu->processor.Get(),frame->surface->view.Get(),0,1,&stream));
    frame->surface->value=++gpu->serial;
    CHECK(gpu->context4->Signal(gpu->fence11.Get(),frame->surface->value));
    gpu->context->Flush();
    *result=frame.release(); return 0;
}
