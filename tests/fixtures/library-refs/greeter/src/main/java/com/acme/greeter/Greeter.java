package com.acme.greeter;

import java.util.ArrayList;
import java.util.List;

/** Published without a sources jar: only its compiled classes reach the app. */
public class Greeter {
    private final String greeting;

    public Greeter(String greeting) {
        this.greeting = greeting;
    }

    public String greet(String name) {
        return greeting + ", " + name + "!";
    }

    public List<String> greetAll(List<String> names) {
        List<String> greetings = new ArrayList<>();
        for (String name : names) {
            greetings.add(greet(name));
        }
        return greetings;
    }
}
