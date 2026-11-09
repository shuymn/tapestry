// Prints what the source's interpreter (LuaJ, run by the source player)
// gives for each presentation-program operation over a grid of numbers,
// as `op a b = result`, every number as its kind and its IEEE bits, so a
// test can compare the program's numerics with it exactly.
//
// Run with the pinned jar on the class path (see `luaj_table.sh`); the
// output is checked in as `crates/skin-beatoraja/tests/luaj_numerics.txt`.

import org.luaj.vm2.Globals;
import org.luaj.vm2.LuaError;
import org.luaj.vm2.LuaValue;
import org.luaj.vm2.lib.jse.JsePlatform;

public class Probe {
    static final double[] FLOATS = {
        0.0, -0.0, 0.5, -0.5, 1.5, -1.5, 2.0, -7.0, 0.1, 5.5,
        Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, Double.NaN,
        9007199254740992.0, -9007199254740992.0, 4294967296.0, 2147483648.0, 1e300,
    };
    static final long[] INTS = {
        0, 1, -1, 2, -2, 3, -3, 7, -7, 2147483647L, -2147483648L,
        9007199254740991L, -9007199254740991L,
    };

    static String show(LuaValue v) {
        if (v.isint()) {
            return "i:" + v.toint();
        }
        if (v.isnumber()) {
            return "f:" + Long.toHexString(Double.doubleToRawLongBits(v.todouble()));
        }
        return v.isnil() ? "nil" : (v.toboolean() ? "true" : "false");
    }

    static LuaValue num(String s) {
        if (s.startsWith("i:")) {
            long n = Long.parseLong(s.substring(2));
            return (n == (int) n) ? LuaValue.valueOf((int) n) : LuaValue.valueOf((double) n);
        }
        return LuaValue.valueOf(Double.longBitsToDouble(Long.parseUnsignedLong(s.substring(2), 16)));
    }

    static String fbits(float f) {
        return Integer.toHexString(Float.floatToRawIntBits(f));
    }

    interface Op { LuaValue apply(LuaValue a, LuaValue b); }

    public static void main(String[] args) {
        Globals g = JsePlatform.standardGlobals();
        LuaValue math = g.get("math");
        java.util.List<String> vals = new java.util.ArrayList<>();
        for (long n : INTS) vals.add("i:" + n);
        for (double d : FLOATS) vals.add("f:" + Long.toHexString(Double.doubleToRawLongBits(d)));
        String[] bin = {"add", "sub", "mul", "div", "mod", "lt", "le", "eq", "max", "min"};
        Op[] ops = {
            (a, b) -> a.add(b), (a, b) -> a.sub(b), (a, b) -> a.mul(b), (a, b) -> a.div(b),
            (a, b) -> a.mod(b), (a, b) -> a.lt(b), (a, b) -> a.lteq(b), (a, b) -> a.eq(b),
            (a, b) -> math.get("max").call(a, b), (a, b) -> math.get("min").call(a, b),
        };
        for (int k = 0; k < bin.length; k++) {
            for (String a : vals) {
                for (String b : vals) {
                    String r;
                    try {
                        r = show(ops[k].apply(num(a), num(b)));
                    } catch (LuaError | ArithmeticException e) {
                        r = "error";
                    }
                    System.out.println(bin[k] + " " + a + " " + b + " = " + r);
                }
            }
        }
        // The consumer's conversion of a number's value
        // (`SkinLuaAccessor.java:640-651`), nil and booleans included.
        for (String a : vals) {
            System.out.println("toint " + a + " = i:" + num(a).toint());
        }
        System.out.println("toint nil = i:" + LuaValue.NIL.toint());
        System.out.println("toint true = i:" + LuaValue.TRUE.toint());
        System.out.println("toint false = i:" + LuaValue.FALSE.toint());
        // A slider value's conversion (`SkinLuaAccessor.java:665-673`):
        // each number as the value the source builds from it (a float
        // that is integral becomes an integer, so -0.0 is integer 0), and
        // strings by their bytes, through `todouble` and `d2f`.
        for (String a : vals) {
            LuaValue x = num(a);
            System.out.println("tofloat " + a + " as " + show(x) + " = g:" + fbits(x.tofloat()));
        }
        System.out.println("tofloat nil as nil = g:" + fbits(LuaValue.NIL.tofloat()));
        System.out.println("tofloat true as true = g:" + fbits(LuaValue.TRUE.tofloat()));
        System.out.println("tofloat false as false = g:" + fbits(LuaValue.FALSE.tofloat()));
        StringBuilder longDigits = new StringBuilder();
        for (int i = 0; i < 70; i++) longDigits.append((char) ('1' + i % 9));
        StringBuilder cut = new StringBuilder("1.");
        for (int i = 0; i < 62; i++) cut.append('0');
        cut.append('x');
        String[] strs = {
            "", " ", "  12  ", "	1", "0x10", "0X1f", "0x", "0x-1", "-0x1", "-", "0x[",
            "1.5", "-0.4", "0.0", "-0.0", "-0", "1e3", "1E-3", ".5", "5.", ".", "+1", "1e", "abc",
            "9223372036854775808", "18446744073709551616", cut.toString(), longDigits.toString(),
            "1e400", "3.4028236e38", "-1e-300", "NaN", "Infinity", "0.25", " 0x10 ",
        };
        for (String s : strs) {
            byte[] bytes = s.getBytes(java.nio.charset.StandardCharsets.ISO_8859_1);
            StringBuilder hex = new StringBuilder("x");
            for (byte by : bytes) hex.append(String.format("%02x", by & 0xff));
            // `0x` alone reads past its bytes and throws, which the
            // consumer catches as 0f.
            String r;
            try {
                r = "g:" + fbits(LuaValue.valueOf(bytes).tofloat());
            } catch (RuntimeException e) {
                r = "caught";
            }
            System.out.println("tofloats " + hex + " = " + r);
        }
        String[] un = {"neg", "floor", "ceil", "abs"};
        for (String a : vals) {
            for (String u : un) {
                String r;
                try {
                    LuaValue x = num(a);
                    r = show(u.equals("neg") ? x.neg() : math.get(u).call(x));
                } catch (LuaError | ArithmeticException e) {
                    r = "error";
                }
                System.out.println(u + " " + a + " = " + r);
            }
        }
    }
}
