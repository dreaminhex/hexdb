package com.dreaminhex.hexdb.jdbc;

import java.util.ArrayList;
import java.util.List;

/**
 * JDBC escape sequences, rewritten into HexDB SQL as the ODBC driver does:
 * {d 'x'}, {t 'x'} and {ts 'x'} become 'x' (HexDB stores dates as text),
 * {fn f(...)} becomes f(...), {oj ...} becomes the join inside it, and
 * {escape 'c'} becomes ESCAPE 'c'. Braces inside literals, quoted names and
 * comments are left alone.
 */
final class Escapes {
    private Escapes() {
    }

    /** Calls `visit` with each character outside literals, quoted names and comments. */
    interface Visitor {
        void visit(int at, char c);
    }

    static void scan(String sql, Visitor visit) {
        int i = 0;
        int n = sql.length();
        while (i < n) {
            char c = sql.charAt(i);
            if (c == '\'' || c == '"' || c == '`') {
                i++;
                while (i < n) {
                    if (sql.charAt(i) == c) {
                        if (i + 1 < n && sql.charAt(i + 1) == c) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i++;
                }
            } else if (c == '-' && i + 1 < n && sql.charAt(i + 1) == '-') {
                while (i < n && sql.charAt(i) != '\n') {
                    i++;
                }
            } else if (c == '/' && i + 1 < n && sql.charAt(i + 1) == '*') {
                i += 2;
                while (i + 1 < n && !(sql.charAt(i) == '*' && sql.charAt(i + 1) == '/')) {
                    i++;
                }
                i++;
            } else {
                visit.visit(i, c);
            }
            i++;
        }
    }

    /** Number of ? parameter markers. */
    static int countParameters(String sql) {
        int[] n = {0};
        scan(sql, (at, c) -> {
            if (c == '?') {
                n[0]++;
            }
        });
        return n[0];
    }

    static String rewrite(String sql) {
        List<int[]> pairs = new ArrayList<>();
        List<Integer> opens = new ArrayList<>();
        scan(sql, (at, c) -> {
            if (c == '{') {
                opens.add(at);
            } else if (c == '}' && !opens.isEmpty()) {
                pairs.add(new int[] {opens.remove(opens.size() - 1), at});
            }
        });
        if (pairs.isEmpty()) {
            return sql;
        }
        // (start, end exclusive, replacement)
        List<Object[]> edits = new ArrayList<>();
        for (int[] pair : pairs) {
            String inner = sql.substring(pair[0] + 1, pair[1]);
            String trimmed = inner.stripLeading();
            int lead = inner.length() - trimmed.length();
            int w = 0;
            while (w < trimmed.length() && Character.isLetter(trimmed.charAt(w)) && trimmed.charAt(w) < 128) {
                w++;
            }
            String word = trimmed.substring(0, w).toLowerCase(java.util.Locale.ROOT);
            String prefix;
            switch (word) {
                case "d":
                case "t":
                case "ts":
                case "fn":
                case "oj":
                    prefix = "";
                    break;
                case "escape":
                    prefix = "ESCAPE";
                    break;
                default:
                    continue;
            }
            edits.add(new Object[] {pair[0], pair[0] + 1 + lead + w, prefix});
            edits.add(new Object[] {pair[1], pair[1] + 1, ""});
        }
        edits.sort((a, b) -> Integer.compare((Integer) a[0], (Integer) b[0]));
        StringBuilder out = new StringBuilder(sql.length());
        int i = 0;
        int skipUntil = 0;
        for (Object[] edit : edits) {
            int start = (Integer) edit[0];
            int end = (Integer) edit[1];
            if (start < skipUntil) {
                continue;
            }
            out.append(sql, i, start).append((String) edit[2]);
            i = end;
            skipUntil = end;
        }
        out.append(sql.substring(i));
        return out.toString();
    }

    /** A metadata search pattern: % any run, _ one character, \ escapes. */
    static boolean matches(String pattern, String text) {
        if (pattern == null) {
            return true;
        }
        return matches(pattern, 0, text, 0);
    }

    private static boolean matches(String p, int pi, String t, int ti) {
        if (pi == p.length()) {
            return ti == t.length();
        }
        char c = p.charAt(pi);
        if (c == '%') {
            for (int i = ti; i <= t.length(); i++) {
                if (matches(p, pi + 1, t, i)) {
                    return true;
                }
            }
            return false;
        }
        if (c == '_') {
            return ti < t.length() && matches(p, pi + 1, t, ti + 1);
        }
        if (c == '\\' && pi + 1 < p.length()) {
            return ti < t.length() && t.charAt(ti) == p.charAt(pi + 1) && matches(p, pi + 2, t, ti + 1);
        }
        return ti < t.length() && t.charAt(ti) == c && matches(p, pi + 1, t, ti + 1);
    }
}
