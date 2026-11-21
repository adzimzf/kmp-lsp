/*
 * Copyright (c) 2026 kmp-lsp contributors.
 *
 * Folding playground for the Java side of kmp-lsp. Every construct below is
 * something an editor should be able to fold. Open this file and try
 * "fold all", "fold level 1..7" and "unfold all"; each block should collapse
 * to its first line and never swallow the line that follows it.
 */
package com.demo.folding;

// Imports: one fold from the first import to the last, comments included.
import java.io.BufferedReader;
import java.io.IOException;
import java.io.StringReader;
import java.util.ArrayList;
// a comment in the middle of the imports
import java.util.Arrays;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.concurrent.Callable;
import java.util.function.Function;
import java.util.function.Supplier;
import java.util.stream.Collectors;

import static java.util.Objects.requireNonNull;
import static java.util.stream.Collectors.toList;

/**
 * Class Javadoc: folds as a comment.
 *
 * <pre>{@code
 * FoldingPlayground playground = new FoldingPlayground("demo");
 * playground.run();
 * }</pre>
 *
 * @param <T> the element type
 */
@SuppressWarnings({
        "unused",
        "rawtypes",
        "unchecked"
})
public class FoldingPlayground<T extends Comparable<T>>
        extends AbstractBase
        implements Runnable, Comparable<FoldingPlayground<T>> {

    // A run of line comments on their own lines
    // folds as one comment region,
    // three lines long.

    // region Constants
    private static final int LIMIT = 10;
    private static final String NAME = "playground"; // trailing comment: not a run
    private static final int[][] MATRIX = {
            {1, 2, 3},
            {
                    4, 5, 6
            },
            {7, 8, 9}
    };
    private static final List<String> WORDS = Arrays.asList(
            "alpha",
            "beta",
            "gamma");
    // endregion

    // <editor-fold desc="Fields">
    /**
     * Field Javadoc.
     */
    private final String name;
    private final List<T> items = new ArrayList<>();
    private final Map<String, List<Integer>> index = new HashMap<>();
    // </editor-fold>

    static {
        System.out.println("static initializer");
        for (int i = 0; i < LIMIT; i++) {
            if (i % 2 == 0) {
                System.out.println(i);
            }
        }
    }

    static { System.out.println("one-line static initializer"); }

    {
        System.out.println("instance initializer");
    }

    /**
     * Constructor Javadoc.
     *
     * @param name the name
     */
    public FoldingPlayground(String name) {
        super(name);
        this.name = requireNonNull(name);
    }

    public FoldingPlayground(
            String name,
            List<T> initial) {
        this(name);
        items.addAll(initial);
    }

    @Override
    public void run() {
        // level 1: method body
        for (T item : items) {
            // level 2: for-each
            if (item != null) {
                // level 3: if
                try {
                    // level 4: try
                    synchronized (this) {
                        // level 5: synchronized
                        int counter = 0;
                        while (counter < LIMIT) {
                            // level 6: while
                            do {
                                // level 7: do-while
                                counter++;
                                if (counter == 5) {
                                    // level 8: innermost
                                    System.out.println("deep");
                                }
                            } while (counter % 3 != 0);
                        }
                    }
                } catch (IllegalStateException | IllegalArgumentException e) {
                    System.err.println(e.getMessage());
                } finally {
                    System.out.println("finally");
                }
            } else if (items.isEmpty()) {
                System.out.println("empty");
            } else {
                System.out.println("null item");
            }
        }
    }

    /** One-line Javadoc: does not fold. */
    public int classicSwitch(int value) {
        int result;
        switch (value) {
            case 1:
                result = 10;
                break;

            case 2:
            case 3:
                result = 20;
                break;
            case 4: {
                int local = value * 2;
                result = local;
                break;
            }
            default:
                result = 0;
        }
        return result;
    }

    public String arrowSwitch(Shape shape) {
        switch (shape.kind()) {
            case CIRCLE -> {
                System.out.println("circle");
                return "round";
            }
            case SQUARE, RECTANGLE -> {
                return "angular";
            }
            default -> throw new IllegalArgumentException(
                    "unknown");
        }
    }

    public int switchExpression(Kind kind) {
        int sides = switch (kind) {
            case CIRCLE -> 0;
            case SQUARE, RECTANGLE -> {
                int base = 2;
                yield base * 2;
            }
            default -> {
                yield -1;
            }
        };
        return sides;
    }

    public String patternSwitch(Object object) {
        return switch (object) {
            case Integer number when number > 10 -> {
                String big = "big " + number;
                yield big;
            }
            case Integer number -> "small " + number;
            case String text -> {
                yield text.toUpperCase();
            }
            case null, default -> "other";
        };
    }

    public String patternInstanceof(Object object) {
        if (object instanceof Circle(double radius)) {
            return "circle " + radius;
        } else if (object instanceof String text
                && !text.isEmpty()) {
            return text;
        }
        return "";
    }

    public List<String> lambdasAndStreams() {
        Function<String, String> shout = word -> {
            String upper = word.toUpperCase();
            return upper + "!";
        };
        Supplier<Runnable> nested = () -> () -> {
            System.out.println("lambda in a lambda");
        };
        Callable<Integer> anonymousInLambda = () -> {
            Comparable<Integer> comparable = new Comparable<>() {
                @Override
                public int compareTo(Integer other) {
                    return 0;
                }
            };
            return comparable.compareTo(1);
        };
        return WORDS.stream()
                .filter(word -> {
                    return word.length() > 3;
                })
                .map(shout)
                .sorted((left, right) -> {
                    int byLength = Integer.compare(left.length(), right.length());
                    if (byLength != 0) {
                        return byLength;
                    }
                    return left.compareTo(right);
                })
                .collect(toList());
    }

    public String textBlock() {
        String json = """
                {
                  "name": "playground",
                  "items": [1, 2, 3]
                }
                """;
        return json.strip();
    }

    public void tryWithResources() throws IOException {
        try (BufferedReader reader = new BufferedReader(
                new StringReader("a\nb"));
             BufferedReader other = new BufferedReader(new StringReader("c"))) {
            String line;
            while ((line = reader.readLine()) != null) {
                System.out.println(line + other.readLine());
            }
        } catch (IOException e) {
            throw e;
        }
    }

    public void labeledLoops() {
        outer:
        for (int row = 0; row < MATRIX.length; row++) {
            inner:
            for (int column = 0; column < MATRIX[row].length; column++) {
                if (MATRIX[row][column] == 5) {
                    break outer;
                }
                if (column == 0) {
                    continue inner;
                }
            }
        }
    }

    public void emptyAndOneLineBlocks() {
        if (items.isEmpty()) { return; }
        if (items.size() == 1) {
        }
        for (;;) { break; }
        Runnable empty = () -> {};
    }

    public void allmanStyle(boolean flag)
    {
        if (flag)
        {
            System.out.println("brace on its own line");
        }
        else
        {
            System.out.println("else on its own line");
        }
    }

    public <R extends Comparable<R>> Optional<R> genericMethod(
            Function<? super T, ? extends R> mapper,
            R fallback,
            String... labels) {
        /*
         * A block comment inside a method body.
         */
        Optional<R> best = items.stream()
                .<R>map(mapper::apply)
                .max(R::compareTo);
        return best.or(() -> Optional.ofNullable(fallback));
    }

    public void localTypes() {
        class LocalClass {
            int value() {
                return 1;
            }
        }
        record LocalRecord(int value) {
            LocalRecord {
                if (value < 0) {
                    throw new IllegalArgumentException();
                }
            }
        }
        enum LocalEnum {
            ONE, TWO
        }
        interface LocalInterface {
            void act();
        }
        System.out.println(new LocalClass().value() + new LocalRecord(1).value()
                + LocalEnum.ONE.ordinal());
    }

    @Override
    public int compareTo(FoldingPlayground<T> other) {
        return name.compareTo(other.name);
    }

    @Override
    protected void hook() {
        System.out.println("hook");
    }

    // ----- nested types ---------------------------------------------------

    /**
     * Static nested class with its own nesting.
     */
    public static class Outer {
        private final Inner inner = new Inner();

        public class Inner {
            public class Innermost {
                void deepest() {
                    Runnable runnable = new Runnable() {
                        @Override
                        public void run() {
                            System.out.println("anonymous at level 5");
                        }
                    };
                    runnable.run();
                }
            }
        }
    }

    /** An interface with default, static and private methods. */
    public interface Shape {
        Kind kind();

        default String describe() {
            return "shape " + kind();
        }

        static Shape circle() {
            return () -> Kind.CIRCLE;
        }

        private void helper() {
            System.out.println("helper");
        }
    }

    /** An enum whose constants have bodies. */
    public enum Kind {
        CIRCLE {
            @Override
            int corners() {
                return 0;
            }
        },
        SQUARE {
            @Override
            int corners() {
                return 4;
            }
        },
        RECTANGLE(
                "rect") {
            @Override
            int corners() {
                return 4;
            }
        };

        private final String label;

        Kind() {
            this("");
        }

        Kind(String label) {
            this.label = label;
        }

        abstract int corners();
    }

    /** A sealed hierarchy of records. */
    public sealed interface Figure permits Circle, Square {
    }

    public record Circle(double radius) implements Figure {
        public Circle {
            if (radius < 0) {
                throw new IllegalArgumentException("negative");
            }
        }

        public double area() {
            return Math.PI * radius * radius;
        }
    }

    public record Square(
            double side,
            String label) implements Figure {
    }

    /** An annotation type with an array default. */
    public @interface Marker {
        String[] value() default {
                "first",
                "second"
        };

        int priority() default 0;
    }
}

/** A second top-level type in the same file. */
abstract class AbstractBase {
    protected final String id;

    protected AbstractBase(String id) {
        this.id = id;
    }

    protected abstract void hook();
}
