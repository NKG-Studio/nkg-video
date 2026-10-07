#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/imgutils.h>
#include <libavutil/opt.h>
#include <libswscale/swscale.h>
#include <libswresample/swresample.h>
#include <math.h>
#include <stdio.h>
#include <string.h>
int nkg_gpu_open(void *, void **, AVBufferRef **, char *, int);
void nkg_gpu_close(void *);
int nkg_gpu_convert(void *, AVFrame *, int, void **, char *, int);

// FFmpeg layouts stay in C and are compiled against the shipped SDK headers.
// Rust owns this opaque handle on one decoder thread for its entire lifetime.
typedef struct NkgDecoder {
    AVFormatContext *input;
    AVCodecContext *codec;
    AVPacket *packet;
    AVFrame *frame, *cpu;
    struct SwsContext *scale;
    SwrContext *resample;
    uint8_t *buffer, *rotated;
    unsigned int capacity, rotated_capacity;
    int stream, draining, hardware, rotation;
    int scale_width, scale_height, scale_format;
    int color_space, color_range;
    double origin, target, audio_cursor;
    char error[256];
    void *gpu;
} NkgDecoder;

static int fail(NkgDecoder *d, int code, const char *where) {
    char detail[128];
    av_strerror(code, detail, sizeof(detail));
    snprintf(d->error, sizeof(d->error), "%s: %s", where, detail);
    return -1;
}
const char *nkg_error(NkgDecoder *d) { return d->error; }
void nkg_close(NkgDecoder *d) {
    if (!d) return;
    sws_freeContext(d->scale);
    swr_free(&d->resample);
    av_free(d->buffer);
    av_free(d->rotated);
    av_frame_free(&d->frame);
    av_frame_free(&d->cpu);
    av_packet_free(&d->packet);
    avcodec_free_context(&d->codec);
    nkg_gpu_close(d->gpu);
    avformat_close_input(&d->input);
    av_free(d);
}
static enum AVPixelFormat hardware_format(AVCodecContext *ctx, const enum AVPixelFormat *formats) {
    (void)ctx;
    for (; *formats != AV_PIX_FMT_NONE; ++formats)
        if (*formats == AV_PIX_FMT_D3D11) return *formats;
    return AV_PIX_FMT_NONE; // Never silently turn a hardware session into software.
}
NkgDecoder *nkg_open(const char *path, int stream, int hardware, const char *decoder_name, int rotation, void *gpu_device, char *error, int error_len) {
    int ret;
    NkgDecoder *d = av_mallocz(sizeof(*d));
    if (!d) { snprintf(error,error_len,"Out of memory"); return NULL; }
    d->stream = stream; d->hardware = hardware; d->rotation = rotation;
    if ((ret = avformat_open_input(&d->input,path,NULL,NULL)) < 0) { fail(d,ret,"open input"); goto failed; }
    if ((ret = avformat_find_stream_info(d->input,NULL)) < 0) { fail(d,ret,"stream info"); goto failed; }
    if (stream < 0 || (unsigned)stream >= d->input->nb_streams) { fail(d,AVERROR(EINVAL),"stream index"); goto failed; }
    AVStream *s = d->input->streams[stream];
    const AVCodec *codec = decoder_name && *decoder_name ? avcodec_find_decoder_by_name(decoder_name) : avcodec_find_decoder(s->codecpar->codec_id);
    // The default AV1 decoder may be libdav1d, which has no hardware configs.
    if (hardware) codec=avcodec_find_decoder_by_name(avcodec_get_name(s->codecpar->codec_id));
    if (!codec) { fail(d,AVERROR_DECODER_NOT_FOUND,"decoder"); goto failed; }
    d->codec = avcodec_alloc_context3(codec);
    d->packet = av_packet_alloc(); d->frame = av_frame_alloc(); d->cpu = av_frame_alloc();
    if (!d->codec || !d->packet || !d->frame || !d->cpu) { fail(d,AVERROR(ENOMEM),"allocate decoder"); goto failed; }
    if ((ret=avcodec_parameters_to_context(d->codec,s->codecpar)) < 0) { fail(d,ret,"codec parameters"); goto failed; }
    d->codec->pkt_timebase = s->time_base;
    // Bound software frame threading to avoid a long startup/reorder queue.
    d->codec->thread_count = hardware ? 1 : 4;
    if (hardware) {
        int supported = 0;
        for (int i=0;;i++) {
            const AVCodecHWConfig *config = avcodec_get_hw_config(codec,i);
            if (!config) break;
            if (config->device_type == AV_HWDEVICE_TYPE_D3D11VA && (config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) { supported=1; break; }
        }
        if (!supported) { fail(d,AVERROR(ENOSYS),"D3D11VA unsupported"); goto failed; }
        d->codec->get_format = hardware_format;
        if (gpu_device) {
            if (nkg_gpu_open(gpu_device,&d->gpu,&d->codec->hw_device_ctx,d->error,sizeof(d->error))<0) goto failed;
        } else if ((ret=av_hwdevice_ctx_create(&d->codec->hw_device_ctx,AV_HWDEVICE_TYPE_D3D11VA,NULL,NULL,0)) < 0) { fail(d,ret,"D3D11VA device"); goto failed; }
    }
    if ((ret=avcodec_open2(d->codec,codec,NULL)) < 0) { fail(d,ret,"open codec"); goto failed; }
    d->origin = d->input->start_time == AV_NOPTS_VALUE ? 0 : (double)d->input->start_time / AV_TIME_BASE;
    if (s->codecpar->codec_type == AVMEDIA_TYPE_AUDIO) {
        AVChannelLayout stereo = AV_CHANNEL_LAYOUT_STEREO;
        if ((ret=swr_alloc_set_opts2(&d->resample,&stereo,AV_SAMPLE_FMT_FLT,48000,&d->codec->ch_layout,d->codec->sample_fmt,d->codec->sample_rate,0,NULL)) < 0 || (ret=swr_init(d->resample)) < 0) { fail(d,ret,"audio resampler"); goto failed; }
    }
    return d;
failed:
    snprintf(error,error_len,"%s",d->error); nkg_close(d); return NULL;
}
int nkg_seek(NkgDecoder *d, double seconds) {
    if (!isfinite(seconds) || seconds < 0) return fail(d,AVERROR(EINVAL),"seek time");
    AVStream *s = d->input->streams[d->stream];
    double seek_time=seconds;
    if (d->resample) {
        double preroll=fmax(0.1,(double)s->codecpar->seek_preroll/d->codec->sample_rate);
        seek_time=fmax(0,seconds-preroll);
    }
    int64_t timestamp = (int64_t)((seek_time+d->origin) / av_q2d(s->time_base));
    int ret = avformat_seek_file(d->input,d->stream,INT64_MIN,timestamp,timestamp,0);
    if (ret < 0) return fail(d,ret,"seek");
    avcodec_flush_buffers(d->codec);
    av_packet_unref(d->packet); av_frame_unref(d->frame); av_frame_unref(d->cpu);
    if (d->resample) { swr_close(d->resample); if ((ret=swr_init(d->resample))<0) return fail(d,ret,"reset resampler"); }
    d->draining=0; d->target=seconds; d->audio_cursor=seconds; d->error[0]=0;
    return 0;
}
static int decode(NkgDecoder *d) {
    av_frame_unref(d->frame);
    for (;;) {
        int ret = avcodec_receive_frame(d->codec,d->frame);
        if (ret >= 0) return 1;
        if (ret == AVERROR_EOF) return 0;
        if (ret != AVERROR(EAGAIN)) return fail(d,ret,"receive frame");
        if (d->draining) return 0;
        do {
            av_packet_unref(d->packet);
            ret = av_read_frame(d->input,d->packet);
        } while (ret >= 0 && d->packet->stream_index != d->stream);
        if (ret == AVERROR_EOF) { d->draining=1; ret=avcodec_send_packet(d->codec,NULL); }
        else if (ret >= 0) { ret=avcodec_send_packet(d->codec,d->packet); av_packet_unref(d->packet); }
        else return fail(d,ret,"read packet");
        if (ret < 0 && ret != AVERROR_EOF) return fail(d,ret,"send packet");
    }
}
static double frame_time(NkgDecoder *d) {
    int64_t pts = d->frame->best_effort_timestamp;
    return pts == AV_NOPTS_VALUE ? d->target : pts * av_q2d(d->input->streams[d->stream]->time_base)-d->origin;
}
// Find the actual predecessor by presentation timestamp, including VFR.
// Only decode during the search; defer readback/conversion until the selected frame.
int nkg_previous(NkgDecoder *d, double before) {
    if (!isfinite(before) || before < 0) return fail(d,AVERROR(EINVAL),"previous frame time");
    // Start near the predecessor: a whole second of raw 4K video can be
    // hundreds of MB. FPS only seeds the search, actual PTS selects the frame;
    // sparse/VFR streams expand backwards until a real predecessor is found.
    AVRational rate=av_guess_frame_rate(d->input,d->input->streams[d->stream],NULL);
    double initial=rate.num>0 && rate.den>0 ? 2.0*av_q2d(av_inv_q(rate)) : 1.0;
    for (double span=initial;;span*=2.0) {
        double start=fmax(0,before-span), previous=-1;
        if (nkg_seek(d,start)<0) return -1;
        int ret;
        while ((ret=decode(d))>0) {
            double pts=frame_time(d);
            if (pts+0.000001>=before) break;
            if (pts>=0) previous=pts;
        }
        if (ret<0) return ret;
        if (previous>=0) return nkg_seek(d,previous);
        if (start==0) return nkg_seek(d,0);
    }
}
int nkg_last(NkgDecoder *d, double duration) {
    if (!isfinite(duration) || duration<0) return fail(d,AVERROR(EINVAL),"end time");
    for (double span=1.0;;span*=2.0) {
        double start=fmax(0,duration-span), last=-1;
        if (nkg_seek(d,start)<0) { if (start==0) return -1; else continue; }
        int ret;
        while ((ret=decode(d))>0) last=frame_time(d);
        if (ret<0) return ret;
        if (last>=0) return nkg_seek(d,last);
        if (start==0) return fail(d,AVERROR_EOF,"no final frame");
    }
}
int nkg_gpu_next(NkgDecoder *d, void **output, int *width, int *height, double *pts) {
    int ret;
    do { ret=decode(d); if(ret<=0) return ret; *pts=frame_time(d); } while(*pts+0.000001<d->target);
    if (!d->gpu || d->frame->format!=AV_PIX_FMT_D3D11) return fail(d,AVERROR(EINVAL),"GPU frame required");
    int w=d->frame->width,h=d->frame->height;
    if(w<=0 || h<=0 || (int64_t)w*h*4>256*1024*1024) return fail(d,AVERROR(EINVAL),"frame dimensions");
    *width=(d->rotation==90||d->rotation==270)?h:w;
    *height=(d->rotation==90||d->rotation==270)?w:h;
    if(nkg_gpu_convert(d->gpu,d->frame,d->rotation,output,d->error,sizeof(d->error))<0) return -1;
    return 1;
}
int nkg_video_next(NkgDecoder *d, const uint8_t **pixels, int *width, int *height, double *pts) {
    int ret;
    // Discard preroll before GPU readback and RGB conversion.
    do { ret=decode(d); if (ret<=0) return ret; *pts=frame_time(d); } while (*pts+0.000001 < d->target);
    AVFrame *frame=d->frame;
    if (d->hardware) {
        if (frame->format != AV_PIX_FMT_D3D11) return fail(d,AVERROR(EINVAL),"expected hardware frame");
        if (d->cpu->width!=frame->width || d->cpu->height!=frame->height) av_frame_unref(d->cpu);
        if ((ret=av_hwframe_transfer_data(d->cpu,frame,0))<0) return fail(d,ret,"GPU readback");
        frame=d->cpu;
    }
    int w=frame->width,h=frame->height;
    if (w<=0 || h<=0 || (int64_t)w*h*4 > 256*1024*1024) return fail(d,AVERROR(EINVAL),"frame dimensions");
    av_fast_malloc(&d->buffer,&d->capacity,(size_t)w*h*4);
    if (!d->buffer) return fail(d,AVERROR(ENOMEM),"RGBA buffer");
    if (!d->scale || d->scale_width!=w || d->scale_height!=h || d->scale_format!=frame->format) {
        sws_freeContext(d->scale);
        d->scale=sws_alloc_context();
        if (!d->scale) return fail(d,AVERROR(ENOMEM),"color converter");
        av_opt_set_int(d->scale,"srcw",w,0); av_opt_set_int(d->scale,"srch",h,0);
        av_opt_set_int(d->scale,"src_format",frame->format,0);
        av_opt_set_int(d->scale,"dstw",w,0); av_opt_set_int(d->scale,"dsth",h,0);
        av_opt_set_int(d->scale,"dst_format",AV_PIX_FMT_RGBA,0);
        av_opt_set_int(d->scale,"sws_flags",SWS_BILINEAR,0);
        av_opt_set_int(d->scale,"threads",4,0);
        if ((ret=sws_init_context(d->scale,NULL,NULL))<0) { sws_freeContext(d->scale); d->scale=NULL; return fail(d,ret,"init color converter"); }
        d->scale_width=w; d->scale_height=h; d->scale_format=frame->format;
        d->color_space=-1; d->color_range=-1;
    }
    if (!d->scale) return fail(d,AVERROR(ENOMEM),"color converter");
    const int *coeff=sws_getCoefficients(d->frame->colorspace == AVCOL_SPC_BT709 ? SWS_CS_ITU709 : SWS_CS_DEFAULT);
    if (d->color_space!=d->frame->colorspace || d->color_range!=d->frame->color_range) {
        if ((ret=sws_setColorspaceDetails(d->scale,coeff,d->frame->color_range==AVCOL_RANGE_JPEG,coeff,1,0,1<<16,1<<16))<0) return fail(d,ret,"color metadata");
        d->color_space=d->frame->colorspace; d->color_range=d->frame->color_range;
    }
    uint8_t *dst[4]={d->buffer,NULL,NULL,NULL}; int stride[4]={w*4,0,0,0};
    if ((ret=sws_scale(d->scale,(const uint8_t *const *)frame->data,frame->linesize,0,h,dst,stride))<0) return fail(d,ret,"RGBA conversion");
    *width=w; *height=h; *pixels=d->buffer;
    if (d->rotation==90 || d->rotation==180 || d->rotation==270) {
        av_fast_malloc(&d->rotated,&d->rotated_capacity,(size_t)w*h*4);
        if (!d->rotated) return fail(d,AVERROR(ENOMEM),"rotation buffer");
        if (d->rotation!=180) { *width=h; *height=w; }
        for (int y=0;y<h;y++) for (int x=0;x<w;x++) {
            int dx,dy;
            if (d->rotation==90) { dx=y; dy=w-1-x; }
            else if (d->rotation==270) { dx=h-1-y; dy=x; }
            else { dx=w-1-x; dy=h-1-y; }
            memcpy(d->rotated+((size_t)dy*(*width)+dx)*4,d->buffer+((size_t)y*w+x)*4,4);
        }
        *pixels=d->rotated;
    }
    return 1;
}
int nkg_audio_next(NkgDecoder *d, const float **samples, int *count, double *pts) {
    for (;;) {
        int ret=decode(d);
        if (ret<0) return ret;
        int flushing=ret==0;
        double start=flushing ? d->audio_cursor : frame_time(d) - (double)swr_get_delay(d->resample,d->codec->sample_rate)/d->codec->sample_rate;
        int capacity=swr_get_out_samples(d->resample,flushing?0:d->frame->nb_samples);
        if (capacity<=0) return 0;
        if (capacity>48000*10) return fail(d,AVERROR(EINVAL),"audio frame too large");
        av_fast_malloc(&d->buffer,&d->capacity,(size_t)capacity*2*sizeof(float));
        if (!d->buffer) return fail(d,AVERROR(ENOMEM),"PCM buffer");
        uint8_t *out[1]={d->buffer};
        ret=swr_convert(d->resample,out,capacity,flushing?NULL:(const uint8_t **)d->frame->extended_data,flushing?0:d->frame->nb_samples);
        if (ret<0) return fail(d,ret,"audio convert");
        d->audio_cursor=start+(double)ret/48000;
        if (!ret) { if (flushing) return 0; else continue; }
        int skip=(int)fmax(0,ceil((d->target-start)*48000-0.00001));
        if (skip>=ret) continue;
        *samples=(float *)d->buffer+skip*2; *count=(ret-skip)*2; *pts=start+(double)skip/48000;
        return 1;
    }
}
