package com.acme.app;

import com.acme.model.User;
import com.acme.registry.Registry;
import com.acme.util.StringUtilsKt;
import com.acme.util.TextHelper;

public class JavaClient {
    public String describe(User user) {
        String name = user.getName();
        user.setEmail("someone@example.com");
        String email = user.getEmail();
        boolean active = user.isActive();
        user.setNickname("nick");
        return name + email + active;
    }

    public User fallback() {
        return User.Companion.anonymous();
    }

    public void registerAll(User user) {
        Registry.INSTANCE.register(user);
        Registry.INSTANCE.removeAll();
    }

    public String textStats(String input) {
        String slug = StringUtilsKt.slugify(input);
        String loud = StringUtilsKt.shout(slug);
        int words = TextHelper.wordCount(input);
        return slug + loud + words;
    }
}
