#define RS "RootConstants(num32BitConstants=8, b0), UAV(u0)"
cbuffer C : register(b0) { uint width; uint height; uint pitch16; uint at16; uint pitch32; uint at32; uint toDepth; uint unused; };
RWByteAddressBuffer buf : register(u0);
uint d24(uint u) { return u * 256 + (u * 255 + 32767) / 65535; }
uint u16(uint d) {
    d &= 0xffffff;
    uint u = min(d >> 8, 65535);
    uint best = u;
    int err = abs((int)d24(u) - (int)d);
    if (u > 0 && abs((int)d24(u - 1) - (int)d) < err) { best = u - 1; err = abs((int)d24(u - 1) - (int)d); }
    if (u < 65535 && abs((int)d24(u + 1) - (int)d) < err) { best = u + 1; }
    return best;
}
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x * 2;
    uint y = id.y;
    if (x >= width || y >= height) return;
    uint a = at16 + y * pitch16 + x * 2;
    uint b = at32 + y * pitch32 + x * 4;
    bool two = x + 1 < width;
    if (toDepth != 0) {
        uint pair = buf.Load(a);
        buf.Store(b, d24(pair & 0xffff));
        if (two) buf.Store(b + 4, d24(pair >> 16));
    } else {
        uint lo = u16(buf.Load(b));
        uint hi = two ? u16(buf.Load(b + 4)) : 0;
        buf.Store(a, lo | hi << 16);
    }
}
