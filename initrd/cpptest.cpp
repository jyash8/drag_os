#include <iostream>
#include <string>
#include <vector>
#include <stdexcept>
int main() {
    std::vector<std::string> v{"hello", "c++", "world"};
    try {
        throw std::runtime_error("caught exception");
    } catch (const std::exception& e) {
        std::cout << "exception: " << e.what() << "\n";
    }
    for (const auto& s : v) std::cout << s << " ";
    std::cout << "sum=" << (1 + 2) << "\n";
    return 0;
}
