#![crate_name = "snapshot"]
#![crate_type = "rlib"]
#![crate_type = "dylib"]

#![feature(unsafe_destructor)]

use std::cell::UnsafeCell;
use std::sync::{ StaticRwLock, StaticMutex, RW_LOCK_INIT, MUTEX_INIT, Arc };
use std::marker::Sync;
use std::iter::IntoIterator;
use std::ops::{ Deref, DerefMut, Drop };

///////////////////////////////////////////////////////////////////////////////
//                                                                           //
//                             Read Write Vec                                //                               
//                                                                           //
///////////////////////////////////////////////////////////////////////////////

struct RWVec<T> {
    rw_lock   : Box<StaticRwLock>,
    push_lock : Box<StaticMutex>,
    data      : UnsafeCell<std::vec::Vec<T>>
}

unsafe impl<T : Send> Sync for RWVec<T> { }

impl<T> RWVec<T> {
    pub fn new() -> Arc<RWVec<T>> {
        Arc::new(RWVec {  
            rw_lock   : Box::new(RW_LOCK_INIT),
            push_lock : Box::new(MUTEX_INIT),
            data      : UnsafeCell::new(std::vec::Vec::new())
        })
    }

    pub fn with_capacity(capacity : usize) -> Arc<RWVec<T>> {
        Arc::new(RWVec {  
            rw_lock   : Box::new(RW_LOCK_INIT),
            push_lock : Box::new(MUTEX_INIT),
            data      : UnsafeCell::new(std::vec::Vec::with_capacity(capacity))
        })
    }

    pub fn push(&mut self, t : T) {
        //compete with other pushers
        unsafe { self.push_lock.lock.lock(); }
        
        //the push will cause a realloc
        if self.data.value.capacity() == self.data.value.len() {
            //compete with other pushers and all the readers as well
            unsafe { self.rw_lock.lock.write(); }
            //push reallocs underlying mem and copys over old values
            self.data.value.push(t);

            unsafe { 
                //safe to read
                self.rw_lock.lock.write_unlock();
                //safe to push again
                self.push_lock.lock.unlock();
            }

            return
        }
        
        //push that doesnt affect reads
        (&mut *self.data.get()).push(t);
        //safe to push again
        unsafe { self.push_lock.lock.unlock(); }
    }

    pub fn reader(&self) -> SliceGuard<T> {
        //return a view of the current snapshot 
        SliceGuard::new(&*self.data.get(), &self.rw_lock, &self.push_lock)
    }
    
    pub fn writer(&mut self) -> SliceGuardMut<T> {
        //return a mutable, upgradable view of the current snapshot 
        SliceGuardMut::new(&*self.data.get(), &self.rw_lock, &self.push_lock)
    }
}

#[unsafe_destructor]
impl<T> Drop for RWVec<T> {
    fn drop(&mut self) {
        unsafe { self.rw_lock.lock.destroy() }
        unsafe { self.push_lock.lock.destroy() }
    }
}

///////////////////////////////////////////////////////////////////////////////
//                                                                           //
//                             IMMUTABLE GUARD                               //                               
//                                                                           //
///////////////////////////////////////////////////////////////////////////////

//multiple read access to a slice representing the current
//state of the Vec...pushers can still push on the vec as long as they don't 
//need to reallocate
struct SliceGuard<'locked, T : 'locked> {
    //the underlying vec
    vec         : &'locked std::vec::Vec<T>,
    //how far to slice on deref...pushers may have corrupted past here
    end         : usize,
    //unlock on drop
    resize_lock : &'locked Box<StaticRwLock>,
    //in case we need to refresh this needs to be accuired
    push_lock   : &'locked Box<StaticMutex>
}   

impl<'locked, T> SliceGuard<'locked, T> {
    fn new(vec : &'locked std::vec::Vec<T>, resize_lock :  &'locked Box<StaticRwLock>, push_lock : &'locked Box<StaticMutex>) -> SliceGuard<'locked, T> {
        unsafe { resize_lock.lock.read() }

        SliceGuard {
            vec         : vec,
            end         : vec.len(),
            resize_lock : resize_lock,
            push_lock   : push_lock
        }   
    }

    //this updates your view of the vec by yielding and then acquiring both locks
    fn refresh(&mut self) { 
        unsafe {
            //give the pending reallocating pushers a chance to finish so no deadlock
            self.resize_lock.lock.read_unlock(); 
            //seal off the pushers
            self.push_lock.lock.lock();
            //register yourself as a reader again
            self.resize_lock.lock.read(); 
        }

        self.end = self.vec.len();

        unsafe {
            //let non-reallocating pushers in again
            self.push_lock.lock.unlock();
        } 
    }
}

impl<'locked, T> IntoIterator for &'locked SliceGuard<'locked, T> {
    type IntoIter = std::slice::Iter<'locked, T>;

    fn into_iter(self) -> std::slice::Iter<'locked, T> {
        //the deref on the functin call delegates this to the slice
        self.iter()
    }
}

impl<'locked, T> Deref for SliceGuard<'locked, T> {
    type Target = [T];

    fn deref<'a>(&'a self) -> &'a [T] {
        &self.vec[..self.end]
    }
}

#[unsafe_destructor]
impl<'locked, T> Drop for SliceGuard<'locked, T> { 
    fn drop(&mut self) {
        self.resize_lock.lock.read_unlock();
    }
}

///////////////////////////////////////////////////////////////////////////////
//                                                                           //
//                             MUTABLE GUARDS                                //                               
//                                                                           //
///////////////////////////////////////////////////////////////////////////////

//Exlusive read and write access to a slice representing the current
//state of the Vec...pushers can still push on the vec as long as they don't 
//need to reallocate
struct SliceGuardMut<'locked, T : 'locked> {
    //the underlying vec
    vec         : &'locked std::vec::Vec<T>,
    //how far to slice on deref...pushers may have corrupted past here
    end         : usize,
    //unlock on drop
    resize_lock : &'locked Box<StaticRwLock>,
    //in case we need to upgrade this needs to be accuired
    push_lock   : &'locked Box<StaticMutex>
}   

impl<'locked, T> SliceGuardMut<'locked, T> {
    fn new(vec: &'locked std::vec::Vec<T>, resize_lock : &'locked Box<StaticRwLock>, push_lock : &'locked Box<StaticMutex>) -> SliceGuardMut<'locked, T> {
        unsafe { resize_lock.lock.write() }

        SliceGuardMut {
            //the underlying vec
            vec         : vec,
            //how far to slice on deref...pushers may have corrupted past here
            end         : vec.len(),
            //unlock on drop
            resize_lock : resize_lock,
            //in case we need to upgrade this needs to be accuired
            push_lock   : push_lock
        }   
    }

    //this updates your view of the vec by yielding and then acquiring both locks
    fn refresh(&mut self) { 
        unsafe {
            //release pushers waiting to realloc
            self.resize_lock.lock.write_unlock();

            //seal off pushers
            self.push_lock.lock.lock();

            //wait for immutable readers to be dropped then lock out new ones
            self.resize_lock.lock.write();
        }

        self.end = self.vec.len();

        unsafe {
            //let non-reallocating pushers in again
            self.push_lock.lock.unlock();
        } 
    }

    //this acquires the push lock as well so you have exclusive access
    //this is basically a scoped version of refresh that lets you exclusively mutate the whole vec 
    //until the guard drops
    fn upgrade(&self) -> VecGuardMut<T> { 
        unsafe {
            //give the pending reallocating pushers a chance to finish so no deadlock
            self.resize_lock.lock.write_unlock(); 
            //seal off the pushers by creating a vec guard
            let vec_guard = VecGuardMut::new(self.vec, self.push_lock);
            //seal off any other reader
            self.resize_lock.lock.write(); 

            vec_guard
        }
    }
}

impl<'locked, T> IntoIterator for &'locked SliceGuardMut<'locked, T> {
    type IntoIter = std::slice::Iter<'locked, T>;

    fn into_iter(self) -> std::slice::Iter<'locked, T> {
        //the deref on the functin call delegates this to the slice
        self.iter()
    }
}

impl<'locked, T> IntoIterator for &'locked mut SliceGuardMut<'locked, T> {
    type IntoIter = std::slice::IterMut<'locked, T>;

    fn into_iter(self) -> std::slice::IterMut<'locked, T> {
        //the deref on the functin call delegates this to the slice
        self.into_iter()
    }
}

impl<'locked, T> Deref for SliceGuardMut<'locked, T> {
    type Target = [T];

    fn deref<'a>(&'a self) -> &'a [T] {
        &self.vec[..self.end]
    }
}

impl<'locked, T> DerefMut for SliceGuardMut<'locked, T> {
    fn deref_mut<'a>(&'a mut self) -> &'a mut [T] {
        &mut self.vec[..self.end]
    }
}

#[unsafe_destructor]
impl<'locked, T> Drop for SliceGuardMut<'locked, T> { 
    fn drop(&mut self) {
        unsafe { self.resize_lock.lock.write_unlock(); }
    }
}

//Exclusive read and write acces to the whole vec...pushers get blocked while
//they wait for this to drop
struct VecGuardMut<'locked, T : 'locked> {
    //exclusive access to the vec
    vec    : &'locked std::vec::Vec<T>,
    //unlock this on drop 
    lock   : &'locked Box<StaticMutex>
}

impl<'locked, T> VecGuardMut<'locked, T> {
    fn new(vec : &'locked std::vec::Vec<T>, push_lock : &'locked Box<StaticMutex>) -> VecGuardMut<'locked, T> {
        unsafe { push_lock.lock.lock() }

        VecGuardMut {
            vec : vec,
            lock : push_lock
        }
    }
}

impl<'locked, T> IntoIterator for &'locked VecGuardMut<'locked, T> {
    type IntoIter = std::slice::Iter<'locked, T>;

    fn into_iter(self) -> std::slice::Iter<'locked, T> {
        //the deref on the functin call delegates this to the vec
        self.into_iter()
    }
}

impl<'locked, T> IntoIterator for &'locked mut VecGuardMut<'locked, T> {
    type IntoIter = std::slice::IterMut<'locked, T>;

    fn into_iter(self) -> std::slice::IterMut<'locked, T> {
        //the deref on the functin call delegates this to the vec
        self.into_iter()
    }
}

impl<'locked, T> Deref for VecGuardMut<'locked, T> {
    type Target = std::vec::Vec<T>;

    fn deref<'a>(&'a self) -> &'a std::vec::Vec<T> {
        self.vec
    }
}


impl<'locked, T> DerefMut for VecGuardMut<'locked, T> {
    fn deref_mut<'a>(&'a mut self) -> &'a mut std::vec::Vec<T> {
        &mut *self.vec
    }
}

#[unsafe_destructor]
impl<'locked, T> Drop for VecGuardMut<'locked, T> { 
    fn drop(&mut self) {
        self.lock.lock.unlock();
    }
}

///////////////////////////////////////////////////////////////////////////////
//                                                                           //
//                                 TESTS                                     //                               
//                                                                           //
///////////////////////////////////////////////////////////////////////////////

// #[test]
// fn basic() {
//     let rwvec = Arc::new(RWVec::with_capacity(20));

//     //spinoff a bunch of pushers that push a specific amount at random times
//     for _ in 0..20 {
//         let vec = rwvec.clone();
//         Thread::spawn(move || {
//             //sleep for a random amount of time
            
//             vec.push(5i)
//         });
//     }

//     //spinoff a bunch of immutable readers that live for an arbitrary amount of time
//     for _ in 0..20 {
//         let vec = rwvec.clone();
//         Thread::spawn(move || {
//             //sleep for a random amount of time
            
//             let reader = vec.reader();
//             for i in &reader {
//                 assert!(i == &5i);
//                 print!("{}", i);
//             }

//             print!("\n");
//         });
//     }

//     //get writer here...put in a specific value...upgrade...change all other values to that one
//     {
//         let mut vec = rwvec.clone();
//         let mut writer = vec.writer();

//         writer[0] = 0i;

//         let writer = writer.upgrade();
//         for val in &mut writer.iter().skip(1) {
//             *val = 10i;
//         }
//     }
//     //drop writer and get a new reader...verify that the contents add up to the right thing

//     //spinoff more pushers...refresh the reader...verify that it changed
// }