//! R8 colour coordinates. Pair components from the same wire response.
use super::{Lighting,Setting};
use anyhow::{Result,bail,ensure};
use serde_json::Value as Json;
use zigbee_zcl::value::Value;

pub(super) fn setting(v:&Json)->Result<Setting>{
    let map=v.as_object().filter(|m|m.len()==2).ok_or_else(||anyhow::anyhow!("color needs x/y or hue/saturation"))?;
    if let Some((x,y))=map.get("x").and_then(Json::as_f64).zip(map.get("y").and_then(Json::as_f64)){
        let wire=|n:f64|->Result<u16>{ensure!((0.0..=1.0).contains(&n),"xy coordinate outside 0..1");
            let raw=(n*65536.).round();ensure!(raw<=65279.,"xy coordinate outside ZCL range");Ok(raw as u16)};
        return Ok(Setting::Xy(wire(x)?,wire(y)?));
    }
    if let Some((h,s))=map.get("hue").and_then(Json::as_f64).zip(map.get("saturation").and_then(Json::as_f64)){
        ensure!((0.0..=360.).contains(&h) && (0.0..=100.).contains(&s),"invalid hue or saturation");
        return Ok(Setting::Hs((h*254./360.).round() as u8,(s*254./100.).round() as u8));
    }
    bail!("color needs x/y or hue/saturation")
}

impl Lighting<'_>{
    pub fn xy(&self)->bool {self.endpoint.has_standard_server(0x300) && self.color_capabilities.is_some_and(|c|c!=0xffff && c&8!=0)}
    pub fn hs(&self)->bool {self.endpoint.has_standard_server(0x300) && self.color_capabilities.is_some_and(|c|c!=0xffff && c&1!=0)}
    pub(super) fn coordinates<'a>(&self,values:impl Iterator<Item=(u16,Value<'a>)>)->Option<Setting>{
        let (mut x,mut y,mut h,mut s,mut mode)=(None,None,None,None,None);
        for (attr,v) in values{
            match (attr,v.data_type().id(),v.as_bytes()){
                (3|4,0x21,[a,b])=>{let n=u16::from_le_bytes([*a,*b]);if n<=0xfeff && (if attr==3{&mut x}else{&mut y}).replace(n).is_some(){return None;}}
                (0|1,0x20,[n @ 0..=254])=>if (if attr==0{&mut h}else{&mut s}).replace(*n).is_some(){return None;},
                (8,0x30,[n])=>mode=Some(*n),_=>{}
            }
        }
        if self.xy() && mode.is_none_or(|m|m==1) && let Some((x,y))=x.zip(y){Some(Setting::Xy(x,y))}
        else if self.hs() && mode.is_none_or(|m|m==0) && let Some((h,s))=h.zip(s){Some(Setting::Hs(h,s))}
        else {None}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Endpoint,lighting::Property};
    use serde_json::json;
    use zigbee_zcl::Frame;
    #[test]
    fn native_coordinates_require_the_mode_and_a_complete_read(){
        let ep=Endpoint{id:11,profile:0x104,servers:&[6,8,0x300],clients:&[]};
        let lamp=Lighting::new(&ep,Some(0x19));
        let xy=setting(&json!({"x":0.5,"y":0.25})).unwrap();
        let r=lamp.set(xy,3,19).unwrap();
        assert_eq!(&r.frame[..9],&[1,19,7,0,128,0,64,3,0]);
        assert!(Lighting::new(&ep,Some(16)).set(xy,0,1).is_err());
        assert!(setting(&json!({"x":1.3,"y":0.5})).is_err());
        let raw=[8,19,1,3,0,0,0x21,0,128,4,0,0,0x21,0,64,8,0,0,0x30,1];
        assert_eq!(lamp.read_response(Property::Color,19,0x300,Frame::decode(&raw).unwrap()).unwrap(),Some(xy));
        assert_eq!(lamp.read_response(Property::Color,19,0x300,Frame::decode(&raw[..9]).unwrap()).unwrap(),None);
    }
}
