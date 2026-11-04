package com.acme.textkit;

public final class LegacyFormatter {
    private LegacyFormatter() {}

    public static String format(String text) {
        return "[" + Normalizer.INSTANCE.normalize(text) + "]";
    }
}
