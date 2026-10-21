/** Bootstrap-visible logger for the instrumented carver hooks (no game types). */
public class CarveLog {
    public static void pre(int tx, int tz, int sx, int sz, int k, long seed) {
        System.out.println("APRE t=" + tx + "," + tz + " s=" + sx + "," + sz + " k=" + k + " seed=" + seed);
    }
    public static void post(int tx, int tz, int sx, int sz, int k, boolean start) {
        System.out.println("APOST t=" + tx + "," + tz + " s=" + sx + "," + sz + " k=" + k + " start=" + start);
    }
    public static void state(int x, int y, int z, int id) {
        System.out.println("ASTATE " + x + " " + y + " " + z + " " + id);
    }
}
