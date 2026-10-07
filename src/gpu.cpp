#include <d3d11_4.h>
#include <d3d12.h>
#include <dxgi1_4.h>
#include <wrl/client.h>
#include <memory>
#include <cstdio>
extern "C" {
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>
#include <libavutil/frame.h>
}
using Microsoft::WRL::ComPtr;
struct GpuFrame {
    ComPtr<ID3D12Resource> resource;
    ComPtr<ID3D12Fence> fence;
    UINT64 value=0;
    int width=0, height=0;
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
    ComPtr<ID3D12Fence> fence12;
    ComPtr<ID3D11Fence> fence11;
    UINT64 serial=0;
    int width=0,height=0;
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
extern "C" void *nkg_gpu_resource(void *p) {
    auto resource=static_cast<GpuFrame *>(p)->resource.Get(); resource->AddRef(); return resource;
}
extern "C" int nkg_gpu_wait(void *p, void *queue) {
    auto frame=static_cast<GpuFrame *>(p);
    return FAILED(static_cast<ID3D12CommandQueue *>(queue)->Wait(frame->fence.Get(),frame->value)) ? -1 : 0;
}
extern "C" int nkg_gpu_convert(void *p, AVFrame *source, int rotation, void **result, char *message, int length) {
    auto gpu=static_cast<GpuDecoder *>(p);
    const int w=source->width,h=source->height;
    if (!gpu->processor || gpu->width!=w || gpu->height!=h) {
        gpu->processor.Reset(); gpu->enumerator.Reset();
        D3D11_VIDEO_PROCESSOR_CONTENT_DESC desc={};
        desc.InputFrameFormat=D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
        desc.InputWidth=w; desc.InputHeight=h;
        desc.OutputWidth=(rotation==90||rotation==270)?h:w;
        desc.OutputHeight=(rotation==90||rotation==270)?w:h;
        desc.Usage=D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;
        CHECK(gpu->video->CreateVideoProcessorEnumerator(&desc,&gpu->enumerator));
        CHECK(gpu->video->CreateVideoProcessor(gpu->enumerator.Get(),0,&gpu->processor));
        gpu->width=w; gpu->height=h;
    }
    auto frame=std::make_unique<GpuFrame>();
    frame->width=(rotation==90||rotation==270)?h:w;
    frame->height=(rotation==90||rotation==270)?w:h;
    D3D12_HEAP_PROPERTIES heap={}; heap.Type=D3D12_HEAP_TYPE_DEFAULT;
    D3D12_RESOURCE_DESC desc={}; desc.Dimension=D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    desc.Width=frame->width; desc.Height=frame->height; desc.DepthOrArraySize=1; desc.MipLevels=1;
    desc.Format=DXGI_FORMAT_B8G8R8A8_UNORM; desc.SampleDesc.Count=1;
    desc.Flags=D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET|D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS;
    CHECK(gpu->output->CreateCommittedResource(&heap,D3D12_HEAP_FLAG_SHARED,&desc,
        D3D12_RESOURCE_STATE_COMMON,nullptr,IID_PPV_ARGS(&frame->resource)));
    HANDLE handle=nullptr;
    CHECK(gpu->output->CreateSharedHandle(frame->resource.Get(),nullptr,GENERIC_ALL,nullptr,&handle));
    ComPtr<ID3D11Device1> device1; HRESULT hr=gpu->device.As(&device1);
    ComPtr<ID3D11Texture2D> output;
    if (SUCCEEDED(hr)) hr=device1->OpenSharedResource1(handle,IID_PPV_ARGS(&output));
    CloseHandle(handle); CHECK(hr);
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC input_desc={};
    input_desc.ViewDimension=D3D11_VPIV_DIMENSION_TEXTURE2D;
    input_desc.Texture2D.ArraySlice=static_cast<UINT>(reinterpret_cast<uintptr_t>(source->data[1]));
    ComPtr<ID3D11VideoProcessorInputView> input_view;
    CHECK(gpu->video->CreateVideoProcessorInputView(reinterpret_cast<ID3D11Texture2D *>(source->data[0]),gpu->enumerator.Get(),&input_desc,&input_view));
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC output_desc={};
    output_desc.ViewDimension=D3D11_VPOV_DIMENSION_TEXTURE2D;
    ComPtr<ID3D11VideoProcessorOutputView> output_view;
    CHECK(gpu->video->CreateVideoProcessorOutputView(output.Get(),gpu->enumerator.Get(),&output_desc,&output_view));
    const bool full=source->color_range==AVCOL_RANGE_JPEG, bt709=source->colorspace==AVCOL_SPC_BT709;
    DXGI_COLOR_SPACE_TYPE color=bt709 ? (full?DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709:DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709)
        : (full?DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601:DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601);
    gpu->video_context->VideoProcessorSetStreamColorSpace1(gpu->processor.Get(),0,color);
    gpu->video_context->VideoProcessorSetOutputColorSpace1(gpu->processor.Get(),DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
    gpu->video_context->VideoProcessorSetStreamAutoProcessingMode(gpu->processor.Get(),0,FALSE);
    gpu->video_context->VideoProcessorSetStreamFrameFormat(gpu->processor.Get(),0,D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
    RECT src={0,0,w,h}, dst={0,0,frame->width,frame->height};
    gpu->video_context->VideoProcessorSetStreamSourceRect(gpu->processor.Get(),0,TRUE,&src);
    gpu->video_context->VideoProcessorSetStreamDestRect(gpu->processor.Get(),0,TRUE,&dst);
    gpu->video_context->VideoProcessorSetOutputTargetRect(gpu->processor.Get(),TRUE,&dst);
    gpu->video_context->VideoProcessorSetStreamRotation(gpu->processor.Get(),0,rotation!=0,
        rotation==90?D3D11_VIDEO_PROCESSOR_ROTATION_270:rotation==270?D3D11_VIDEO_PROCESSOR_ROTATION_90:rotation==180?D3D11_VIDEO_PROCESSOR_ROTATION_180:D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY);
    D3D11_VIDEO_PROCESSOR_STREAM stream={}; stream.Enable=TRUE; stream.pInputSurface=input_view.Get();
    CHECK(gpu->video_context->VideoProcessorBlt(gpu->processor.Get(),output_view.Get(),0,1,&stream));
    frame->fence=gpu->fence12; frame->value=++gpu->serial;
    CHECK(gpu->context4->Signal(gpu->fence11.Get(),frame->value));
    gpu->context->Flush();
    *result=frame.release(); return 0;
}
