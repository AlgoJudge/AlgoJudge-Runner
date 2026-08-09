// The checker contract, verbatim from SIO2.
//
//   argv[1]  the input file
//   argv[2]  the participant's output
//   argv[3]  the reference output
//
//   stdout line 1  OK or WRONG
//   stdout line 2  optional comment, shown to the participant
//   stdout line 3  optional integer 0-100, the percentage of the test's points
//   exit code      ALWAYS 0
//
// The exit code is the load-bearing rule: a non-zero code means this checker
// failed, not that the answer was wrong. Every path below therefore ends at
// `return 0`, including the ones that could not read a file.

#include <fstream>
#include <iostream>
#include <string>

// `<fstream>` is on the header deny-list for **submissions** and is entirely
// legitimate here: a checker is code the problem author wrote, not code a
// participant sent. It is still run in its own sandbox with its own limits.

int main(int argc, char** argv) {
    if (argc < 4) {
        // Not the participant's fault, and not reported as their failure: the
        // Runner reads a non-zero exit as a broken checker, so this says WRONG
        // with a comment rather than exiting non-zero and being unreadable.
        std::cout << "WRONG\ncheckera wywolano bez trzech plikow\n";
        return 0;
    }

    std::ifstream answer(argv[2]);
    std::ifstream expected(argv[3]);

    long long theirs = 0, ours = 0;
    if (!(expected >> ours)) {
        std::cout << "WRONG\nnie udalo sie odczytac oczekiwanego wyniku\n";
        return 0;
    }
    if (!(answer >> theirs)) {
        std::cout << "WRONG\nbrak liczby na wyjsciu\n";
        return 0;
    }

    // Anything after the number is output the task did not ask for.
    std::string trailing;
    if (answer >> trailing) {
        std::cout << "WRONG\nna wyjsciu jest cos wiecej niz jedna liczba\n";
        return 0;
    }

    if (theirs != ours) {
        std::cout << "WRONG\noczekiwano innej sumy\n";
        return 0;
    }

    std::cout << "OK\n";
    return 0;
}
