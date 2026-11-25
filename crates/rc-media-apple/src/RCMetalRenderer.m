#import "RCMetalRenderer.h"

@implementation RCMetalRenderer {
    id<MTLDevice> _device;
    id<MTLCommandQueue> _queue;
    id<MTLRenderPipelineState> _pipeline;
    CVMetalTextureCacheRef _cache;
}

- (nullable instancetype)initWithDevice:(id<MTLDevice>)device {
    if (!(self = [super init])) return nil;
    _device = device;
    _queue = [device newCommandQueue];
    CVMetalTextureCacheCreate(NULL, NULL, device, NULL, &_cache);
    NSString *source =
        @"#include <metal_stdlib>\n"
        "using namespace metal;\n"
        "struct Vertex { float4 pos [[position]]; float2 uv; };\n"
        "vertex Vertex vmain(uint i [[vertex_id]]) {\n"
        "    float2 p[3] = {float2(-1,-1), float2(3,-1), float2(-1,3)};\n"
        "    return {float4(p[i],0,1), float2((p[i].x+1)*0.5,(1-p[i].y)*0.5)};\n"
        "}\n"
        "fragment float4 fmain(Vertex in [[stage_in]], texture2d<float> luma [[texture(0)]],"
        "                      texture2d<float> chroma [[texture(1)]], constant bool& full [[buffer(0)]]) {\n"
        "    constexpr sampler s(filter::linear);\n"
        "    float y = luma.sample(s,in.uv).r;\n"
        "    float2 uv = chroma.sample(s,in.uv).rg - 0.5;\n"
        "    if (!full) { y = (y-16.0/255)*(255.0/219); uv *= 255.0/224; }\n"
        "    return float4(y+1.5748*uv.y, y-0.1873*uv.x-0.4681*uv.y, y+1.8556*uv.x, 1);\n"
        "}\n";
    id<MTLLibrary> library = [device newLibraryWithSource:source options:nil error:NULL];
    if (!library || !_queue) return nil;
    MTLRenderPipelineDescriptor *desc = [MTLRenderPipelineDescriptor new];
    desc.vertexFunction = [library newFunctionWithName:@"vmain"];
    desc.fragmentFunction = [library newFunctionWithName:@"fmain"];
    desc.colorAttachments[0].pixelFormat = MTLPixelFormatBGRA8Unorm;
    _pipeline = [device newRenderPipelineStateWithDescriptor:desc error:NULL];
    return _pipeline ? self : nil;
}

- (void)dealloc {
    if (_cache) CFRelease(_cache);
}

- (id<MTLTexture>)texture:(CVPixelBufferRef)frame plane:(size_t)i
                  buffers:(NSMutableArray *)buffers {
    size_t w = CVPixelBufferGetWidthOfPlane(frame, i);
    size_t h = CVPixelBufferGetHeightOfPlane(frame, i);
    MTLPixelFormat format = i ? MTLPixelFormatRG8Unorm : MTLPixelFormatR8Unorm;
    CVMetalTextureRef cv = NULL;
    if (_cache && CVMetalTextureCacheCreateTextureFromImage(NULL, _cache, frame, NULL,
            format, w, h, i, &cv) == kCVReturnSuccess && cv) {
        id<MTLTexture> texture = CVMetalTextureGetTexture(cv);
        [buffers addObject:CFBridgingRelease(cv)];
        if (texture) return texture;
    }
    // Simulator decoder buffers need not have an IOSurface.
    if (CVPixelBufferLockBaseAddress(frame, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess)
        return nil;
    id<MTLTexture> texture = nil;
    void *bytes = CVPixelBufferGetBaseAddressOfPlane(frame, i);
    if (bytes) {
        MTLTextureDescriptor *desc = [MTLTextureDescriptor
            texture2DDescriptorWithPixelFormat:format width:w height:h mipmapped:NO];
        desc.storageMode = MTLStorageModeShared;
        texture = [_device newTextureWithDescriptor:desc];
        [texture replaceRegion:MTLRegionMake2D(0, 0, w, h) mipmapLevel:0
                     withBytes:bytes bytesPerRow:CVPixelBufferGetBytesPerRowOfPlane(frame, i)];
    }
    CVPixelBufferUnlockBaseAddress(frame, kCVPixelBufferLock_ReadOnly);
    return texture;
}

- (BOOL)drawFrame:(CVPixelBufferRef)frame inView:(MTKView *)view viewport:(CGRect)viewport {
    BOOL full = frame && CVPixelBufferGetPixelFormatType(frame) == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
    if (frame && (CGRectIsEmpty(viewport) || CVPixelBufferGetPlaneCount(frame) != 2 ||
                  (!full && CVPixelBufferGetPixelFormatType(frame) != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)))
        return NO;
    MTLRenderPassDescriptor *pass = view.currentRenderPassDescriptor;
    id<CAMetalDrawable> drawable = view.currentDrawable;
    id<MTLCommandBuffer> cmd = [_queue commandBuffer];
    if (!pass || !drawable || !cmd) return NO;
    NSMutableArray *buffers = [NSMutableArray arrayWithCapacity:3];
    id<MTLTexture> y = nil, uv = nil;
    if (frame) {
        y = [self texture:frame plane:0 buffers:buffers];
        uv = [self texture:frame plane:1 buffers:buffers];
        if (!y || !uv) return NO;
        [buffers addObject:(__bridge id)frame];
        [cmd addCompletedHandler:^(id<MTLCommandBuffer> completed) {
            [buffers removeAllObjects];
        }];
    }
    id<MTLRenderCommandEncoder> enc = [cmd renderCommandEncoderWithDescriptor:pass];
    if (!enc) return NO;
    if (frame) {
        [enc setViewport:(MTLViewport){viewport.origin.x, viewport.origin.y,
                                       viewport.size.width, viewport.size.height, 0, 1}];
        [enc setRenderPipelineState:_pipeline];
        [enc setFragmentTexture:y atIndex:0];
        [enc setFragmentTexture:uv atIndex:1];
        [enc setFragmentBytes:&full length:sizeof(full) atIndex:0];
        [enc drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
    }
    [enc endEncoding];
    [cmd presentDrawable:drawable];
    [cmd commit];
    return YES;
}
@end
