//! Three separately colored EQ curves, computed once per settings/rate change.
//! Inspecting them never sends a player command or changes saved settings.
use crate::audio::eq::{Response, Settings};
use crate::gfx::{Color, Rect, Weight};
use crate::input::Event;
use crate::ui::{Action, Ctx, Draw, Screen};

const POINTS: usize = 288;

pub struct Curves {
    settings: Settings,
    response: Option<Response>,
    sample_rate: Option<u32>,
    cursor: usize,
    output: bool,
}
impl Curves {
    pub fn new() -> Self {
        Self {settings:Settings::default(),response:None,sample_rate:None,cursor:163,output:false}
    }
    fn refresh(&mut self,ctx:&Ctx) -> bool {
        let rate=ctx.status.sample_rate.filter(|r|*r>=1000);
        // The output in use: the jack's settings, or a Bluetooth device's own profile.
        let settings=ctx.outputs.eq(&ctx.config.playback.eq);
        if self.response.is_some() && self.settings==settings && self.sample_rate==rate {return false;}
        let old_hz=self.response.as_ref().map_or(1000.0,|r|r.hz[self.cursor]);
        self.settings=settings;
        self.sample_rate=rate;
        let response=Response::new(&self.settings,rate.unwrap_or(48000),POINTS);
        self.cursor=response.hz.partition_point(|f|*f<old_hz).min(POINTS-1);
        self.response=Some(response);
        true
    }
}
fn frequency(hz:f64) -> String {
    if hz>=1000.0 {format!("{:.2} kHz",hz/1000.0)} else {format!("{hz:.0} Hz")}
}
// Compress the logarithmic 20 Hz..20 kHz axis into the buzzer's useful range.
// Pitch follows frequency, independently of boost/cut or headroom display.
fn click_frequency(hz: f64) -> u16 {
    let position = (hz.clamp(20.0, 20000.0) / 20.0).log10() / 3.0;
    (500.0 * 8.0_f64.powf(position)).round() as u16
}
fn chart_y(db:f64,limit:f64,plot:Rect)->i32 {
    plot.y+((limit-db.clamp(-limit,limit))/(2.0*limit)*(plot.h-1) as f64).round() as i32
}
fn curve(d:&mut Draw,plot:Rect,values:&[f64],offset:f64,limit:f64,color:Color) {
    // One frequency sample per pixel. Connect successive points, including
    // steep narrow filters, without constructing drawing paths each frame.
    for (x,pair) in values.windows(2).enumerate() {
        let y0=chart_y(pair[0]+offset,limit,plot);
        let y1=chart_y(pair[1]+offset,limit,plot);
        d.c.fill_rect(Rect::new(plot.x+x as i32,y0.min(y1),2,(y1-y0).abs()+1),color);
    }
}
impl Screen for Curves {
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::widget("Curves", format!("cursor={};output={}", self.cursor, self.output)) }
    fn title(&self)->String {"EQ Curves".into()}
    fn click_frequency(&self) -> Option<u16> {
        self.response.as_ref().map(|r| click_frequency(r.hz[self.cursor]))
    }
    fn enter(&mut self,ctx:&mut Ctx) {self.refresh(ctx);}
    fn tick(&mut self,ctx:&mut Ctx)->bool {self.refresh(ctx)}
    fn event(&mut self,ev:Event,_ctx:&mut Ctx)->Action {
        match ev {
            Event::Scroll(t)=>{self.cursor=(self.cursor as i64+t as i64*3).clamp(0,POINTS as i64-1) as usize;Action::Stay}
            Event::Center=>{self.output=!self.output;Action::Stay}
            _=>Action::None,
        }
    }
    fn draw(&mut self,d:&mut Draw,area:Rect) {
        let Some(response)=&self.response else {return;};
        let cx=area.x+area.w/2;
        let name=self.settings.headphone.as_ref().map_or("Equalizer",|p|p.name.as_str());
        d.fonts.draw_fit(d.c,name,area.x+42,area.y+7,area.w-84,17,Weight::Bold,d.theme.text);
        let colors=if d.theme.dark {[Color(0x6CB6FF),Color(0xFFBB66),Color(0x62D9A6)]}
            else {[Color(0x2468CC),Color(0xB76600),Color(0x148253)]};
        let offset=if self.output {response.preamp_db} else {0.0};
        let names=[if self.settings.headphone_enabled {"Headphone"} else {"Headphone off"},
            if self.settings.graphic_enabled {"Graphic"} else {"Graphic off"},if self.output {"Total output"} else {"Total"}];
        let values=[response.headphone_db[self.cursor],response.graphic_db[self.cursor],response.combined_db[self.cursor]+offset];
        for i in 0..3 {
            let x=area.x+81+i as i32*107;
            d.fonts.draw_centered(d.c,names[i],x,area.y+32,13,Weight::Bold,colors[i]);
            d.fonts.draw_centered(d.c,&format!("{:+.1} dB",values[i]),x,area.y+49,14,Weight::Regular,colors[i]);
        }
        let plot=Rect::new(area.x+44,area.y+77,POINTS as i32,112);
        let peak=response.headphone_db.iter().chain(&response.graphic_db).copied()
            .chain(response.combined_db.iter().map(|v|v+offset)).map(f64::abs).fold(0.0,f64::max);
        let limit=(peak/6.0).ceil().max(2.0)*6.0;
        for db in [-limit,0.0,limit] {
            let y=chart_y(db,limit,plot);
            d.c.fill_rect(Rect::new(plot.x,y,plot.w,1),if db==0.0 {d.theme.muted} else {d.theme.track});
            d.fonts.draw_right(d.c,&format!("{db:+.0}"),plot.x-6,y-7,12,Weight::Regular,d.theme.muted);
        }
        let upper=*response.hz.last().unwrap();
        let x_for=|hz:f64| plot.x+((hz/20.0).ln()/(upper/20.0).ln()*(plot.w-1) as f64).round() as i32;
        for (hz,label) in [(20.0,"20"),(100.0,"100"),(1000.0,"1k"),(10000.0,"10k")] {
            if hz>upper || (hz!=20.0 && plot.right()-x_for(hz)<26) {continue;}
            let x=x_for(hz);
            d.c.fill_rect(Rect::new(x,plot.y,1,plot.h),d.theme.track);
            d.fonts.draw_centered(d.c,label,x,plot.bottom()+5,12,Weight::Regular,d.theme.muted);
        }
        let end=if upper%1000.0==0.0 {format!("{:.0}k",upper/1000.0)} else if upper>=1000.0 {format!("{:.1}k",upper/1000.0)} else {format!("{upper:.0}")};
        d.fonts.draw_centered(d.c,&end,plot.right()-1,plot.bottom()+5,12,Weight::Regular,d.theme.muted);
        let old=d.c.set_clip(plot);
        let x=plot.x+self.cursor as i32;
        for y in (plot.y..plot.bottom()).step_by(4) {d.c.fill_rect(Rect::new(x,y,1,2),d.theme.muted);}
        curve(d,plot,&response.headphone_db,0.0,limit,colors[0]);
        curve(d,plot,&response.graphic_db,0.0,limit,colors[1]);
        curve(d,plot,&response.combined_db,offset,limit,colors[2]);
        for i in 0..3 {
            d.c.fill_circle(x as f32,chart_y(values[i],limit,plot) as f32,3.0,0.0,colors[i]);
        }
        d.c.set_clip(old);
        let rate=self.sample_rate.unwrap_or(48000) as f64/1000.0;
        let rate_text=if self.sample_rate.is_some() {format!("{rate} kHz")} else {"48 kHz preview".into()};
        d.fonts.draw_centered(d.c,&format!("{} · {}",frequency(response.hz[self.cursor]),rate_text),cx,area.y+212,15,Weight::Regular,d.theme.text);
        d.fonts.draw_centered(d.c,&format!("Headroom {:+.1} dB · {}",response.preamp_db,if self.output {"applied to total"} else {"not shown in total"}),cx,area.y+235,13,Weight::Regular,d.theme.muted);
        d.fonts.draw_centered(d.c,"Wheel: inspect · Center: headroom",cx,area.y+256,13,Weight::Regular,d.theme.muted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_click_covers_bass_to_treble_monotonically() {
        assert_eq!(click_frequency(20.0), 500);
        assert_eq!(click_frequency(20000.0), 4000);
        let pitches: Vec<_> = [20.0, 100.0, 1000.0, 10000.0, 20000.0]
            .into_iter().map(click_frequency).collect();
        assert!(pitches.windows(2).all(|p| p[0] < p[1]));
        assert_eq!(click_frequency(1.0), 500);
        assert_eq!(click_frequency(48000.0), 4000);
    }
}
