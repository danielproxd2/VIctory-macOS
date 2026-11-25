#define RS "RootConstants(num32BitConstants=8, b0), UAV(u0)"
cbuffer C : register(b0) { uint width; uint height; uint pitch16; uint at16; uint pitch32; uint at32; uint toDepth; uint unused; };
RWByteAddressBuffer buf : register(u0);
uint depth(uint u) { return asuint((float)u * asfloat(0x37800080)); }
uint u16(uint d) { return (uint)(saturate(asfloat(d)) * 65535.0 + 0.5); }
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
        buf.Store(b, depth(pair & 0xffff));
        if (two) buf.Store(b + 4, depth(pair >> 16));
    } else {
        uint lo = u16(buf.Load(b));
        uint hi = two ? u16(buf.Load(b + 4)) : 0;
        buf.Store(a, lo | hi << 16);
    }
}
