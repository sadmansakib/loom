#[derive(Debug,Clone,Copy,PartialEq)]
pub struct Point{pub x:f64,pub y:f64}
impl Point{
pub fn new(x:f64,y:f64)->Self{Self{x,y}}
pub fn norm(self)->f64{(self.x*self.x+self.y*self.y).sqrt()}
}
pub fn dist(a:Point,b:Point)->f64{(a.x-b.x).hypot(a.y-b.y)}
pub fn farthest<'a>(points:&'a[Point],from:Point)->Option<&'a Point>{points.iter().max_by(|a,b|dist(**a,from).partial_cmp(&dist(**b,from)).expect("finite coordinates"))}
#[cfg(test)]
mod tests{use super::*;
#[test]
fn distance_between_axes(){let a=Point::new(0.0,0.0);let b=Point::new(3.0,4.0);assert!((dist(a,b)-5.0).abs()<1e-9);}
#[test]
fn farthest_pick(){let pts=[Point::new(0.0,0.0),Point::new(1.0,0.0),Point::new(9.0,9.0)];let f=farthest(&pts,Point::new(0.0,0.0)).unwrap();assert_eq!((f.x,f.y),(9.0,9.0));}}
