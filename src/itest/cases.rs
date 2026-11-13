//! One case per request kind: a template for the next one of its kind.

use expect_test::expect;

use super::{check, Kind};

#[test]
fn definition_of_a_method_called_on_a_parameter() {
    check(
        Kind::Definition,
        r#"
//- /com/demo/Circle.java
package com.demo;

class Circle {
    double area() {
        return 3.14;
    }
}
//- /com/demo/Square.java
package com.demo;

class Square {
    double area() {
        return 4.0;
    }
}
//- /com/demo/Main.java
package com.demo;

class Main {
    double measure(Circle circle, Square square) {
        return square.area() + circle.ar$0ea();
    }
}
"#,
        expect![[r#"
            /com/demo/Circle.java:4:12  double area() {
        "#]],
    );
}

#[test]
fn type_definition_of_a_local() {
    check(
        Kind::TypeDefinition,
        r#"
//- /com/demo/Circle.java
package com.demo;

class Circle {
}
//- /com/demo/Main.java
package com.demo;

class Main {
    Circle copy(Circle circle) {
        Circle same = circle;
        return sa$0me;
    }
}
"#,
        expect![[r#"
            /com/demo/Circle.java:3:7  class Circle {
        "#]],
    );
}

#[test]
fn references_to_a_local_skip_the_field_of_the_same_name() {
    check(
        Kind::References,
        r#"
//- /com/demo/Counter.java
package com.demo;

class Counter {
    int count;

    int twice(int start) {
        int count = start;
        count = co$0unt + count;
        return count;
    }

    int field() {
        return count;
    }
}
"#,
        expect![[r#"
            /com/demo/Counter.java:7:13  int count = start;
            /com/demo/Counter.java:8:9  count = count + count;
            /com/demo/Counter.java:8:17  count = count + count;
            /com/demo/Counter.java:8:25  count = count + count;
            /com/demo/Counter.java:9:16  return count;
        "#]],
    );
}

#[test]
fn document_highlight_of_a_parameter() {
    check(
        Kind::DocumentHighlight,
        r#"
//- /com/demo/Scale.java
package com.demo;

class Scale {
    double apply(double factor, double value) {
        double scaled = value * fac$0tor;
        return scaled + factor;
    }
}
"#,
        expect![[r#"
            /com/demo/Scale.java:4:25  double apply(double factor, double value) {
            /com/demo/Scale.java:5:33  double scaled = value * factor;
            /com/demo/Scale.java:6:25  return scaled + factor;
        "#]],
    );
}

#[test]
fn completion_offers_the_receivers_members_not_its_private_ones() {
    check(
        Kind::Completion,
        r#"
//- /com/demo/Circle.java
package com.demo;

class Circle {
    double radius;
    private double cache;

    double area() {
        return 3.14 * radius * radius;
    }

    private void refresh() {
    }

    static Circle unit() {
        return new Circle();
    }
}
//- /com/demo/Main.java
package com.demo;

class Main {
    double measure(Circle circle) {
        return circle.$0
    }
}
"#,
        expect![[r#"
            radius
            area
            unit
        "#]],
    );
}

#[test]
fn hover_shows_the_signature_of_the_overload_called() {
    check(
        Kind::Hover,
        r#"
//- /com/demo/Printer.java
package com.demo;

class Printer {
    void print(int number) {
    }

    void print(String text, int copies) {
    }
}
//- /com/demo/Main.java
package com.demo;

class Main {
    void run(Printer printer) {
        printer.pri$0nt("page", 2);
    }
}
"#,
        expect![[r#"
            void print(String text, int copies)
        "#]],
    );
}

#[test]
fn signature_help_marks_the_argument_being_typed() {
    check(
        Kind::SignatureHelp,
        r#"
//- /com/demo/Printer.java
package com.demo;

class Printer {
    void print(String text, int copies) {
    }
}
//- /com/demo/Main.java
package com.demo;

class Main {
    void run(Printer printer) {
        printer.print("page", 2$0);
    }
}
"#,
        expect![[r#"
            print(String text, [int copies])
        "#]],
    );
}

#[test]
fn prepare_rename_selects_the_whole_identifier() {
    check(
        Kind::PrepareRename,
        r#"
//- /com/demo/Main.java
package com.demo;

class Main {
    int total(int first, int second) {
        int subtotal = first + second;
        return sub$0total;
    }
}
"#,
        expect![[r#"
            /com/demo/Main.java:6:16  return subtotal;
        "#]],
    );
}

#[test]
fn document_symbol_outlines_a_class() {
    check(
        Kind::DocumentSymbol,
        r#"
//- /com/demo/Circle.java
package com.demo;

class Circle$0 {
    static final double PI = 3.14;
    private final double radius;

    Circle(double radius) {
        this.radius = radius;
    }

    double area() {
        return PI * radius * radius;
    }

    enum Unit {
        METRE,
        FOOT
    }
}
"#,
        expect![[r#"
            class Circle
            constant PI
            field radius
            constructor Circle
            method area
            enum Unit
            enum-member METRE
            enum-member FOOT
        "#]],
    );
}

#[test]
fn folding_range_folds_the_class_and_each_body() {
    check(
        Kind::FoldingRange,
        r#"
//- /com/demo/Circle.java
package com.demo;

class Circle$0 {
    double radius;

    double area() {
        double square = radius * radius;
        return 3.14 * square;
    }

    double perimeter() {
        double diameter = radius * 2;
        return 3.14 * diameter;
    }
}
"#,
        expect![[r#"
            3..15
            6..9
            11..14
        "#]],
    );
}

#[test]
fn workspace_symbol_finds_the_types_a_name_matches() {
    check(
        Kind::WorkspaceSymbol,
        r#"
//- /com/demo/Circle.java
package com.demo;

class Circle {
}
//- /com/demo/CircleTest.java
package com.demo;

class CircleTest {
    Circle$0 circle;
}
//- /com/demo/Square.java
package com.demo;

class Square {
}
"#,
        expect![[r#"
            Circle  /com/demo/Circle.java:3
            CircleTest  /com/demo/CircleTest.java:3
        "#]],
    );
}
