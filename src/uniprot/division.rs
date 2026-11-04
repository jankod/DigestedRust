#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(i32)]
pub enum Division {
    Bacteria = 0,
    Invertebrates = 1,
    Mammals = 2,
    Phages = 3,
    PlantsAndFungi = 4,
    Primates = 5,
    Rodents = 6,
    SyntheticAndChimeric = 7,
    Unassigned = 8,
    Viruses = 9,
    Vertebrates = 10,
    EnvironmentalSamples = 11,
}

impl Division {
    pub fn from_taxon(taxon: &str) -> Self {
        match taxon.trim().to_lowercase().as_str() {
            "bacteria" => Division::Bacteria,
            "viruses" => Division::Viruses,
            "primates" => Division::Primates,
            "rodents" => Division::Rodents,
            "mammals" => Division::Mammals,
            "vertebrates" => Division::Vertebrates,
            "invertebrates" => Division::Invertebrates,
            "plants" | "fungi" => Division::PlantsAndFungi,
            "phages" => Division::Phages,
            "environmental samples" => Division::EnvironmentalSamples,
            "synthetic" | "chimeric" => Division::SyntheticAndChimeric,
            _ => Division::Unassigned,
        }
    }

    #[allow(dead_code)]
    pub fn from_id(division_id: i32) -> Self {
        match division_id {
            0 => Division::Bacteria,
            1 => Division::Invertebrates,
            2 => Division::Mammals,
            3 => Division::Phages,
            4 => Division::PlantsAndFungi,
            5 => Division::Primates,
            6 => Division::Rodents,
            7 => Division::SyntheticAndChimeric,
            8 => Division::Unassigned,
            9 => Division::Viruses,
            10 => Division::Vertebrates,
            11 => Division::EnvironmentalSamples,
            _ => Division::Unassigned,
        }
    }

    pub fn id(self) -> i32 {
        self as i32
    }

    #[allow(dead_code)]
    pub fn name(self) -> &'static str {
        match self {
            Division::Bacteria => "Bacteria",
            Division::Invertebrates => "Invertebrates",
            Division::Mammals => "Mammals",
            Division::Phages => "Phages",
            Division::PlantsAndFungi => "Plants and Fungi",
            Division::Primates => "Primates",
            Division::Rodents => "Rodents",
            Division::SyntheticAndChimeric => "Synthetic and Chimeric",
            Division::Unassigned => "Unassigned",
            Division::Viruses => "Viruses",
            Division::Vertebrates => "Vertebrates",
            Division::EnvironmentalSamples => "Environmental samples",
        }
    }
}

pub fn get_division_id(taxon: &str) -> i32 {
    Division::from_taxon(taxon).id()
}
