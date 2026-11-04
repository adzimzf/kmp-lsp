package com.acme.app;

import com.acme.textkit.LegacyFormatter;
import com.acme.textkit.Normalizer;

public class JavaApp {
    public String render(String input) {
        String clean = Normalizer.INSTANCE.normalize(input);
        return LegacyFormatter.format(clean);
    }
}
