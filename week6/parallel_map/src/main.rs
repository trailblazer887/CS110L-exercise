use std::{
    sync::{mpsc, Arc, Mutex},
    thread, time,
};

fn parallel_map<T, U, F>(mut input_vec: Vec<T>, num_threads: usize, f: F) -> Vec<U>
where
    F: FnOnce(T) -> U + Send + Copy + 'static,
    T: Send + 'static,
    U: Send + 'static + Default,
{
    let mut output_vec: Vec<U> = Vec::with_capacity(input_vec.len());
    output_vec.resize_with(input_vec.len(), || U::default());
    // TODO: implement parallel map!
    let (tx, rx) = mpsc::channel();
    let mutex_vec = Arc::new(Mutex::new(input_vec));
    for _ in 0..num_threads {
        let tx = tx.clone();
        let mutex_vec = mutex_vec.clone();
        let _ = thread::spawn(move || loop {
            let mut v = mutex_vec.lock().unwrap();
            let val = v.pop();
            let len = v.len();
            drop(v);
            if let Some(val) = val {
                tx.send((len, f(val))).unwrap();
            } else {
                break;
            }
        });
    }
    drop(tx);
    for (ix, val) in rx {
        output_vec[ix] = val;
    }
    output_vec
}

fn main() {
    let v = vec![6, 7, 8, 9, 10, 1, 2, 3, 4, 5, 12, 18, 11, 5, 20];
    let squares = parallel_map(v, 10, |num| {
        println!("{} squared is {}", num, num * num);
        thread::sleep(time::Duration::from_millis(500));
        num * num
    });
    println!("squares: {:?}", squares);
}
