package com.example;

public class Example {
    // tag::main[]
    public static void main(String[] args) {
        System.out.println("Hello from Java!");
        System.out.println("This demonstrates tag-filtered includes.");
    }
    // end::main[]

    public void helperMethod() {
        // This method should NOT appear when using tags=main
        System.out.println("Helper output");
    }
}
