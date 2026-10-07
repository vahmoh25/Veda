// cxx — checks of the C++ library (libstdc++) on Veda, which programs in
// C++ build on: exceptions, run-time types, static constructors, containers
// and strings, streams, threads.
//
// Each check prints "ok - NAME" or "not ok - NAME"; the program exits with
// the number of failed checks (0: all passed). `systest` runs it inside
// Veda in a scratch directory.

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <exception>
#include <fstream>
#include <iostream>
#include <map>
#include <memory>
#include <mutex>
#include <sstream>
#include <stdexcept>
#include <string>
#include <thread>
#include <typeinfo>
#include <vector>

static int failures;

static void check(const char *name, bool ok)
{
	std::printf("%s - %s\n", ok ? "ok" : "not ok", name);
	if (!ok)
		failures++;
}

// Constructed before main runs.
struct Global {
	int value = 42;
	std::string name = "global";
};
static Global global;

struct Base {
	virtual ~Base() = default;
	virtual int kind() const { return 1; }
};

struct Derived : Base {
	int kind() const override { return 2; }
};

thread_local int per_thread = 1;

static void check_exceptions()
{
	bool caught = false;
	try {
		std::vector<int> v(3);
		(void)v.at(10);
	} catch (const std::out_of_range &) {
		caught = true;
	}
	check("exception from the library", caught);
	caught = false;
	try {
		throw 7;
	} catch (int x) {
		caught = x == 7;
	}
	check("exception of a plain type", caught);

	// One thread throws, another rethrows what it caught.
	std::exception_ptr error;
	std::thread([&] {
		try {
			throw std::runtime_error("from a thread");
		} catch (...) {
			error = std::current_exception();
		}
	}).join();
	caught = false;
	try {
		if (error)
			std::rethrow_exception(error);
	} catch (const std::runtime_error &e) {
		caught = std::string(e.what()) == "from a thread";
	}
	check("exception carried across threads", caught);
}

static void check_types_and_containers()
{
	check("static constructor", global.value == 42 && global.name == "global");
	std::unique_ptr<Base> b = std::make_unique<Derived>();
	check("virtual call, dynamic_cast, typeid",
	      b->kind() == 2 && dynamic_cast<Derived *>(b.get()) && typeid(*b) == typeid(Derived));
	std::map<std::string, int> m{{"one", 1}, {"two", 2}};
	std::string s = "veda";
	s += std::to_string(m["two"]);
	check("containers and strings", s == "veda2" && m.size() == 2);
}

static void check_streams()
{
	std::ostringstream os;
	os << "x=" << 3.5 << ' ' << std::hex << 255;
	check("string stream", os.str() == "x=3.5 ff");
	{
		std::ofstream out("cxx.txt");
		out << "first line\n";
	}
	std::ifstream in("cxx.txt");
	std::string line;
	std::getline(in, line);
	check("file streams", line == "first line");
	std::remove("cxx.txt");
}

static void check_threads()
{
	std::mutex mu;
	std::condition_variable cv;
	int ready = 0;
	std::atomic<int> sum{0};
	std::vector<std::thread> threads;
	for (int i = 0; i < 4; i++)
		threads.emplace_back([&, i] {
			per_thread = 100 + i;
			sum += per_thread;
			std::lock_guard<std::mutex> lock(mu);
			ready++;
			cv.notify_one();
		});
	{
		std::unique_lock<std::mutex> lock(mu);
		cv.wait(lock, [&] { return ready == 4; });
	}
	for (auto &t : threads)
		t.join();
	check("threads, a mutex, a condition variable", sum == 406);
	check("thread_local", per_thread == 1);

	auto start = std::chrono::steady_clock::now();
	std::this_thread::sleep_for(std::chrono::milliseconds(20));
	auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - start);
	check("clocks and sleeping", ms.count() >= 20 && ms.count() < 5000);
}

int main()
{
	check_exceptions();
	check_types_and_containers();
	check_streams();
	check_threads();
	std::cout << (failures ? "FAIL" : "PASS") << ": " << failures << " failed" << std::endl;
	return failures;
}
