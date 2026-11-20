package probe;

import java.nio.*;
import java.util.*;

public final class Probe {
    static long percentile(long[] a, double p) { Arrays.sort(a); return a[(int)Math.ceil(p*a.length)-1]; }
    static long[] samples(Runnable r, int n) { long[] v=new long[n]; for(int i=0;i<n;i++){ long s=System.nanoTime(); r.run(); v[i]=System.nanoTime()-s; } return v; }
    static String stats(long[] a) { return String.format(Locale.ROOT,"p50=%.3f p95=%.3f p99=%.3f",percentile(a,.50)/1000.0,percentile(a,.95)/1000.0,percentile(a,.99)/1000.0); }
    static void tests() {
        byte[] topo=NativeBridge.topology(2000), poses=NativeBridge.posesAndPaletteDiffs(1);
        NativeBridge.require(NativeBridge.call(1,1,topo,2000)==2000,"topology accepted");
        NativeBridge.require(NativeBridge.call(2,1,poses,4)==4,"pose accepted");
        NativeBridge.require(NativeBridge.call(3,1,NativeBridge.logicOutput(),1)==1,"server logic output accepted");
        ByteBuffer bad=NativeBridge.frame(1,2,topo,2000); bad.put(0,(byte)0); NativeBridge.require(NativeBridge.transfer(bad)==-1,"corrupt magic rejected");
        ByteBuffer crcBad=NativeBridge.frame(1,2,topo,2000); crcBad.put(NativeBridge.HEADER+8,(byte)(crcBad.get(NativeBridge.HEADER+8)^1)); NativeBridge.require(NativeBridge.transfer(crcBad)==-2,"corrupt payload rejected");
        ByteBuffer oversized=ByteBuffer.allocateDirect(65537); NativeBridge.require(NativeBridge.transfer(oversized)==-11,"oversized capacity rejected");
        NativeBridge.require(NativeBridge.call(1,1,topo,2000)==-6,"out of order generation rejected");
        NativeBridge.require(NativeBridge.call(1,2,topo,2000)==2000,"next generation accepted");
        System.out.println("tests=PASS cases=8 (A/B/C accept, corrupt header/payload, oversized, stale generation, next generation)");
    }
    public static void main(String[] args) {
        tests();
        byte[] topo=NativeBridge.topology(2000), poses=NativeBridge.posesAndPaletteDiffs(3);
        int warm=2000, n=10000;
        for(int i=0;i<warm;i++){ NativeBridge.call(1,100+i,topo,2000); NativeBridge.call(2,100+i,poses,4); }
        long[] serialA=samples(()->NativeBridge.topology(2000),n);
        long[] decodeA=samples(()->NativeBridge.call(1,100000+System.nanoTime(),topo,2000),n);
        long[] perNode=samples(new Runnable(){ long generation=100_000+System.nanoTime()+1_000_000_000_000L; public void run(){
            for(int i=0;i<2000;i++) NativeBridge.require(NativeBridge.call(1,++generation,Arrays.copyOfRange(topo,i*8,i*8+8),1)==1,"per-node call");
        }},100);
        long[] serialB=samples(()->NativeBridge.poses(3),n);
        long[] decodeB=samples(()->NativeBridge.call(2,100000+System.nanoTime(),poses,4),n);
        long baselineA=2000L*(8+NativeBridge.HEADER), batchA=topo.length+NativeBridge.HEADER;
        long staticBlocks=4L*256*4, poseBytes=4L*52+NativeBridge.HEADER;
        long paletteDiff=4L*8*6, contraptionBatch=poseBytes+paletteDiff;
        System.out.println("scenario_A=kinetic_topology_delta nodes=2000 per_node_frames_bytes="+baselineA+" batch_bytes="+batchA+" ratio="+String.format(Locale.ROOT,"%.2f",(double)baselineA/batchA));
        System.out.println("scenario_A_serialize_us "+stats(serialA));
        System.out.println("scenario_A_rust_validate_us "+stats(decodeA));
        System.out.println("scenario_A_per_node_2000_jni_calls_total_us "+stats(perNode));
        System.out.println("scenario_B=contraption_pose contraptions=4 static_blocks=1024 static_resend_bytes_per_tick="+staticBlocks+" pose_plus_palette_bytes_per_tick="+contraptionBatch);
        System.out.println("scenario_B_serialize_us "+stats(serialB));
        System.out.println("scenario_B_rust_validate_us "+stats(decodeB));
        System.out.println("ring_slots=4 slot_capacity="+NativeBridge.CAPACITY+" footprint_bytes="+(4L*NativeBridge.CAPACITY));
        int c=NativeBridge.call(3,2,NativeBridge.logicOutput(),1);
        NativeBridge.require(c==1,"logic output roundtrip");
        System.out.println("scenario_C=PASS rust_accepted_server_outputs=speed_1.25,stress_units_420,active_slots_6,pose_delta_12_70_-3 client_visual_owner=client_render_mods server_owner=tick_and_collision");
    }
}
